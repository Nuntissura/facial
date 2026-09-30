---
file_id: REF-WP-082-MATCH-STRICT-RECOGNITION-CALIBRATION-SCALE-V1
file_kind: refinement
updated_at: "2026-08-23"
---

<topic id="operator-request" status="active" version="2" wp="WP-082" summary="Optimize Match for extremely low false positives and make every activation statistic independently reproducible from a canonical hashed evidence contract." updated_at="2026-08-23">

## Operator request

The operator explicitly prefers many untagged faces and ID failures over false positives. Match therefore has no minimum auto-assignment recall target. Automatic assignment is statistically eligible only after a frozen local evaluation proves the false-positive gate. Product activation additionally requires the WP-084 correction/shadow-review surface and WP-087 integration gate; otherwise every result remains a suggestion or unidentified. Even after activation, `committed_strict_automatic` remains model-derived and bound to its producing model/calibration generation; it is not `operator_confirmed` evidence and cannot become trusted evidence until the operator explicitly confirms it and separately authorizes trusted enrollment.

The evaluation itself has one machine-readable authority: `governance/validation/wp-082-match-calibration-v1.yaml`. Large source media stays outside the repository under configurable `FACIAL_MATCH_EVAL_ROOT`; the artifact stores relative manifest references, SHA-256 identities, frozen protocol/envelope fields, results, and review verdicts. The verifier route is `facial-cli match_calibration_verify --contract FILE --eval-root DIR`. Missing roots, manifest drift, partition leakage, spent-test reuse, undersized evidence, or missing review fail closed.

## Spec anchors

- Match strict-recognition contract: `specs/app-spec.md` section 17.4.
- Match domain and calibration topology: `topology.yaml` `match.calibration`.

</topic>

<topic id="research-basis" status="active" version="2" wp="WP-082" summary="Large 1:N galleries require local calibration, conservative incremental density grouping, exact reranking, trusted exemplars, explicit hard-negative constraints, and a canonical reproducible evidence graph." updated_at="2026-08-23">

## Sources and selected approach

- InsightFace evaluation guidance: https://www.insightface.ai/guides/choose-face-recognition-model-and-evaluate
- Immich incremental grouping: https://docs.immich.app/features/facial-recognition/
- NIST FRTE 1:N evaluation: https://pages.nist.gov/frvt/html/frvt1N.html
- NIST FRTE paperless-travel 1:N evaluation at FPIR 0.0003: https://pages.nist.gov/frvt/html/frvt_paperless_travel.html
- NIST FRTE demographic evaluation at pairwise FMR 0.00003: https://pages.nist.gov/frvt/html/frvt_demographics.html
- NIST exact-binomial proportion bounds: https://www.itl.nist.gov/div898/software/dataplot/refman1/auxillar/propconf.htm
- HNSW parameter behavior: https://github.com/nmslib/hnswlib/blob/master/ALGO_PARAMS.md

Current implementation verification (2026-08-23) also checked NIST's current FRTE 1:N API/report surfaces, the current SurrealDB 3.2 vector-index reference, the hnswlib source documentation, InsightFace's current 1:N evaluation workflow, and statrs 0.19.0 source/API documentation. NIST still defines FPIR as non-mated searches returning one or more candidates at or above threshold and still reports the paperless-travel 0.0003 operating point. SurrealDB documents HNSW as in-memory, exposes M/EFC plus per-query effort, and recommends `EXPLAIN FULL` to prove index use. hnswlib confirms that higher query effort trades latency for recall and that M/EFC trade memory/build time for graph quality. InsightFace continues to require local 1:N validation and raw similarity/threshold decisions rather than probability claims. Rust `statrs` 0.19.0 provides the inverse beta CDF needed for exact Clopper-Pearson bounds and is compatible with the project's Rust 1.97 toolchain.

Selected implementation: use a typed, hash-reconciled Rust verifier; statrs 0.19.0 for exact beta inversion; the already-selected shared SurrealDB HNSW index only for candidates; exact F32 rerank and per-Look scoring for the final decision; and fail-closed activation on any evidence, envelope, generation, or review mismatch. Rejected options are Wilson/normal bounds (not the exact named gate), pairwise-FMR multiplication as a 1:N proxy (not empirical FPIR), Python/scipy verification (violates the Rust-native product contract), a Person-wide centroid (weakens multi-Look separation), and post-hoc or reused test-set tuning (invalidates activation evidence). Validation requires known exact-bound fixtures, manifest hash/path/leakage attacks, independent recomputation from raw records, deterministic per-Look/cannot-link decisions, and real operator-scale evidence before strict automatic activation.

Use separate unnamed-cluster and known-person thresholds, top-K ANN candidates, exact F32 rerank, runner-up margin, quality/pose gates, bounded trusted references per Look, and persistent face-to-person cannot-links. Search Looks independently, aggregate to one stable Person, and prohibit a single centroid across all of a Person's appearances. Calibrate against the actual shipped Look/template multiplicity. Collapse duplicate families for density evidence. Freeze Person-disjoint calibration/test partitions, prevent every source or derived duplicate family from crossing the split, and report hard slices rather than one benchmark average.

NIST defines open-set FPIR as the fraction of non-mated searches that return one or more candidates above threshold, evaluates paperless-travel systems at FPIR 0.0003, and uses pairwise FMR 0.00003 for a demographic comparison point. NIST also cautions that 1:N behavior must be measured empirically rather than inferred from N pairwise comparisons. The 0.0003 aggregate ceiling is Facial's selected project risk budget, not a NIST mandate.

Match gates three end-to-end statistics independently: the effective combined fraction of emitted `committed_strict_automatic` assignments that name the wrong Person, the effective mated wrong-Person fraction so fixture mixture cannot dilute it, and empirical open-set FPIR through the full shipped pipeline. Their aggregate one-sided 95-percent Clopper-Pearson upper bounds must each be at most 0.0003 and every automatic-eligible hard slice at most 0.001. An effective statistic uses at most one predeclared duplicate-family-collapsed probe per ground-truth Person and at most one from each shared source-asset/capture-session/burst/video-track/duplicate-family/lineage-root acquisition cluster aggregate and per slice; crops and synthetic variations inherit their lineage root. Aggregate gates require at least 10,000 distinct People and acquisition clusters after both caps; slice gates require at least 3,000 of each. Repeated faces, shared group photos, and combinatorial impostor pairs cannot manufacture a denominator. Pairwise FMR is descriptive external context unless every Person and acquisition cluster occurs in at most one predeclared pair for an independent bound; it never enables automatic assignment.

The evidence graph freezes through hashed, evaluation-root-relative manifests for fixtures, People, acquisition clusters, lineage roots, duplicate families, hard slices, partitions, gallery envelope, exact gallery composition, selected probes, and spent activation sets. The canonical result artifact records the complete inputs, metric outputs, insufficiency reasons, and independent review. A purpose-built verifier reconstructs those relationships and calculations rather than trusting self-authored result fields. Pairwise impostor FMR remains required diagnostic evidence but is excluded from the activation-pass conjunction; it cannot substitute for or block the end-to-end product gates.

Calibration and test People are disjoint. Within test, mated enrollment references and probes also come from different source assets, capture sessions, duplicate families, bursts, video tracks, lineage roots, derived crops, and synthetic variations; a non-mated probe Person is absent from the gallery. Once any candidate observes activation-test outcomes, those People and acquisition clusters are spent: later candidates need untouched evidence or a predeclared sequential-testing/alpha-spending design, while reuse is descriptive regression evidence only. The complete supported envelope hashes the exact gallery manifest/composition and freezes maximum People, Looks per Person, trusted templates per Look, total templates, trusted-template selection policy, automatic-commit threshold, ANN index type/version/algorithm/distance metric/quantization/build seed/order/tie-breaking/full index-build and query parameters (including HNSW M, efConstruction, and efSearch where applicable), candidate K, exact-rerank configuration, aggregation, margin, quality, cannot-link, and generation fields. Any hash mismatch, overrun, or field change disables strict automatic commits until recalibration.

The frozen mandatory slice registry is multi-label and includes `lookalike-twin-impostor`, `nonfrontal-pose`, `known-look-age-time-gap`, `styling-makeup`, `wig-hair-change`, `glasses`, `mask-partial-occlusion`, `poor-exposure`, `small-blurred-face`, `compression-resize`, `screenshot`, `collage-poster-multiface`, `synthetic-media`, `duplicate-burst-video-family`, and predeclared `demographic-cohort-*` slices when ground truth is available without product attribute inference. Each definition, ground-truth membership predicate, overlap rule, denominator, and runtime router freezes before test. A lookalike/twin probe qualifies only when at least one different enrolled gallery Person shares its frozen rival-group ID. For an excluded slice, both router-miss rate and final strict-auto-commit escape rate use the same effective-probe denominator and must independently pass the 0.001 one-sided bound over at least 3,000 People and acquisition clusters. A downstream threshold cannot hide a router miss; missing router evidence leaves the whole generation suggestion-only.

Recognition preserves three explicit states: `suggestion` is review-only and creates no assignment; `committed_strict_automatic` is a model-derived Person assignment valid only for its producing model/calibration generation; `operator_confirmed` is durable Person evidence created only by explicit manual assignment or `Same`. Review transitions are exact: `Same` confirms Person evidence but does not authorize trusted enrollment; `Different` rejects/removes the candidate assignment and adds a face-to-candidate-Person cannot-link; `Not sure` changes neither identity truth nor constraints. The ordinary correction label `This is not <name>` invokes the same `Different` transaction.

Any route that supplies only a Person ID enters that Person's `Unsorted` membership. Moving to an existing Look or using `Same person, new look` is explicit. A trusted reference must be operator-confirmed, explicitly assigned to a Look, independently and explicitly authorized for trusted use, and pass alignment, quality, generation, and pose/diversity gates. Passing those gates, confirming identity, or assigning a Look never auto-enrolls it.

</topic>

<topic id="red-team" status="active" version="2" wp="WP-082" summary="Lookalikes, contaminated exemplars, duplicate bursts, evidence drift, threshold overfitting, and performance shortcuts can all manufacture false confidence." updated_at="2026-08-23">

## Risks and controls

- Lookalike contamination: trusted exemplars only, margin gate, cannot-links, conflict quarantine.
- Multi-look averaging: bounded per-Look medoids/templates; no Person-wide centroid; Unsorted observations cannot become trusted references.
- Automatic assignment becomes false operator evidence: preserve model/calibration generation and keep `committed_strict_automatic` outside confirmed and trusted pools until explicit `Same` or manual assignment.
- Eligible observation auto-enrolls as trusted: require independent explicit trusted-reference authorization in addition to confirmation, Look membership, and every eligibility gate.
- Review wording mutates the wrong state: enforce `Same`, `Different`, and `Not sure` as typed transactions and map `This is not <name>` to `Different` semantics.
- Threshold overfit: disjoint frozen test set, named one-sided Clopper-Pearson bounds, and a fully frozen search/gate/gallery envelope.
- Metric substitution: pairwise FMR cannot stand in for empirical 1:N FPIR or wrong emitted automatic assignments.
- Undersized zero-error fixture: require the one-sided upper bound itself to meet the target and fail closed below 10,000 independent aggregate People or 3,000 independent People per slice.
- Paper-only gate with no reproducible evidence: require the canonical contract/result artifact, configurable external evaluation root, hashed manifest graph, independent verifier, and independent review.
- Fixture or spent-set drift after a candidate sees results: bind every manifest hash into the run and reject reuse or mutation before metric evaluation.
- Correlated fixture leakage: use at most one predeclared family-collapsed probe per Person and per source/session/burst/track/family acquisition cluster for each effective statistic, keep source/derived families together, and never treat shared group photos or combinatorial pairs as independent trials.
- Test enrollment leakage: gallery references and probes are independently disjoint by asset, capture session, family, burst, and video track.
- Repeated test-set tuning: an observed activation set is spent; later candidates use untouched People/acquisitions or a frozen sequential alpha-spending design.
- Mated/non-mated mixture dilution: gate mated wrong-Person error and non-mated FPIR separately in addition to the combined product rate.
- Post-hoc easy slices: freeze the stable slice registry, membership predicates, overlap rules, and runtime router before evaluation.
- Duplicate density: exact/perceptual family weighting totals one.
- Similarity presented as probability: label it similarity unless separately calibrated.
- Scale pressure relaxes correctness: performance and accuracy gates remain independent; no shortcut may lower the false-positive bar.

</topic>

<topic id="microtask-plan" status="active" version="2" wp="WP-082" summary="Build the portable evidence contract, fixture manifests, verifier, and metrics before enabling any automatic assignment." updated_at="2026-08-23">

## Microtasks

1. Finalize the canonical artifact schema, configurable evaluation root, hashed manifest graph, and independent verifier route.
2. Define Person-disjoint calibration/test fixtures, within-test enrollment/probe disjointness, independent Person trial selection, source/derived duplicate-family grouping, the stable mandatory hard-slice registry and label predicates, shipped gallery-envelope limits, spent-set registry, and combined/mated/FPIR denominators.
3. Implement duplicate families, quality/pose features, Look-aware trusted-reference selection, independent explicit reference authorization, and a structural ban on eligibility-driven enrollment.
4. Implement conservative clustering, per-Look search, Person aggregation, explicit suggestion/strict-automatic/operator-confirmed states, generation binding, Unsorted membership, review transitions, margins, and constraints.
5. Tune HNSW candidate recall independently from assignment thresholds.
6. Freeze the operating point and prove the combined wrong-auto-assignment, mated wrong-Person, empirical 1:N FPIR, independent-Person sample-sufficiency, excluded-slice abstention, envelope-overrun, Same/Different/Not sure, and trusted-enrollment gates; report pairwise FMR only as non-substitute context.
7. Independently reproduce the artifact's denominators/bounds/verdicts and benchmark warm/bounded inference, memory, throughput, and full-library projection.

</topic>
