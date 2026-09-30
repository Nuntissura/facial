---
file_id: REF-WP-085-MATCH-SEARCH-PORTABILITY-RECOVERY-V1
file_kind: refinement
updated_at: "2026-09-06"
---

<topic id="operator-request" status="active" version="2" wp="WP-085" summary="Make Match identities searchable, bounded-input portable, restorable, relocatable, and removable through explicitly distinct rebuild and clear-all operations without coupling them to original files." updated_at="2026-08-23">

## Scope contract

Add `person:` and negated person terms without changing the existing all-terms-AND search grammar, reusing the stable-ID Person/alias autocomplete catalog from Viewer editing. Provide a versioned Facial identity bundle as the authoritative exchange format, including Looks, `Unsorted`, exact assignment/evidence states and generation provenance, typed review transitions, and independent durable trusted-reference authorization while excluding regenerable vectors. XMP is an optional previewed interoperability projection, sidecar-only by default. Import is treated as hostile bounded input: limits, canonical paths, hashes, schema, stable-ID graph integrity, and container safety are proven before any write transaction.

Reset vocabulary is exact. `rebuild_match_analysis` removes only derived/regenerable Match state and preserves operator-owned truth. `clear_all_match_data` additionally removes operator-owned Match data only after an exact preview, verified versioned recovery bundle, and state-bound confirmation. Both modes preserve raw media.

## Spec anchors

- Match search/exchange/recovery contract: `specs/app-spec.md` section 17.7.
- Match storage and reset topology: `topology.yaml` `match.storage_and_concurrency` and `match.recovery`.

</topic>

<topic id="research-basis" status="active" version="2" wp="WP-085" summary="Stable-ID bounded exchange preserves Facial semantics while IPTC/MWG regions provide limited interop and typed reset modes prevent accidental loss of operator truth." updated_at="2026-09-06">

## Sources and selected approach

- IPTC Photo Metadata 2025.1: https://www.iptc.org/std/photometadata/specification/IPTC-PhotoMetadata-2025.1.html
- MWG region tags: https://exiv2.org/tags-xmp-mwg-rs.html
- PhotoPrism XMP behavior: https://docs.photoprism.app/developer-guide/metadata/xmp/
- SurrealDB manual transactions and per-statement error checking: https://surrealdb.com/docs/reference/rust/concepts/transaction
- serde_json bounded recursive deserialization behavior: https://docs.rs/serde_json/latest/serde_json/struct.Deserializer.html
- Rust zip path-confinement and symlink APIs: https://docs.rs/zip/latest/zip/read/struct.ZipFile.html
- OWASP archive traversal and symlink test cases: https://owasp.org/www-project-web-security-testing-guide/latest/4-Web_Application_Security_Testing/10-Business_Logic_Testing/09-Test_Upload_of_Malicious_Files

Export durable identity data and manifests, including Looks, explicit Look memberships, `Unsorted`, trusted-reference authorization/pins and eligibility evidence, and the distinct `suggestion`, `committed_strict_automatic`, and `operator_confirmed` states while excluding embeddings/crops by default. Suggestions remain unassigned; strict-automatic assignments retain their model/calibration generation and model-derived ownership; import cannot promote either state to operator-confirmed evidence. Person-only assignments remain `Unsorted` until an explicit existing-Look move or `Same person, new look` operation.

Version review operations with exact semantics: `Same` creates operator-confirmed Person evidence without trusted authorization; `Different` rejects/removes and adds the face-to-candidate-Person cannot-link; `Not sure` changes neither identity truth nor constraints; `This is not <name>` is the `Different` transaction. TrustedTemplateSet restoration requires independent explicit authorization, explicit Look membership, and reconciled alignment, quality, generation, pose/diversity, and provenance evidence; eligibility alone never restores or creates membership. Import proceeds through frozen resource ceilings—including separately exposed semantic-reference and JSON-token-work ceilings—canonical bounded parsing, dry-run, stable-ID graph validation, content hashes, transactional application, conflict records, relocation mapping, rollback, and independent reconciliation. Preserve existing indexed/reference search parity and no-OR contract.

The selected v1 container is canonical plain JSON rather than ZIP/TAR: it keeps decoded bytes equal to bounded input bytes and removes archive extraction, compression-ratio, entry-path, and archive-symlink attack surfaces. Compressed/archive magic is rejected explicitly. Parsing retains serde_json's recursion guard, applies a smaller Facial nesting ceiling, and completes byte/count/string/reference/hash/schema/graph checks before the short SurrealDB transaction. ZIP extraction and direct original-file XMP writes were rejected for v1 because neither adds required semantics and both widen the mutation/security surface.

`rebuild_match_analysis` is the ordinary repair path and retains People, Looks, assignments, manual regions, constraints, trust authorization, and history. `clear_all_match_data` is the explicit destructive reset and cannot run until its pre-clear bundle independently proves the durable operator-owned graph exactly restorable, excluded vectors/crops independently rebuildable, and its state-bound confirmation matches the unchanged preview.

## Exact Person counts: correlated query remediation

Research checked 2026-09-06: `product/Cargo.lock` pins `surrealdb-core` 3.2.4. Its `src/exec/physical_expr/subquery.rs`, `ScalarSubquery::evaluate`, executes the inner plan and collects its complete result; `evaluate_batch` invokes that evaluation for each outer row. The previous whole-Person count predicates used uncorrelated `face_id IN (SELECT VALUE face_id ...)` arrays inside two Face scans, allowing repeated full Person-result materialization. This source-level finding explains the costly query shape; it is not a measured engine trace or a completion claim.

The official [SurrealDB correlated semi-join example](https://surrealdb.com/blog/thinking-inside-the-box-relational-style-joins-in-surrealdb) uses `$parent` to restrict each inner query to its outer row and `LIMIT 1` for existence. Select indexed Face-key probes with `array::len(...) > 0`, reusing assignment, suggestion, constraint, template-set and Look indexes and adding Face-leading trusted-search/member indexes. Reject unbounded whole-Person arrays and reduced count ceilings: neither preserves the required bounded work and exact over-limit preview.

Keep the outer canonical Face set and exact distinct Face/media grouping. The union includes source Person assignments, suggestions, cannot-links, trusted-search references and membership through template set to source-owned Look. Merge additionally includes target assignments and suggestions only. Missing Faces contribute nothing; overlapping references count once. Each nested `$parent` binds the immediate enclosing Face, member or template-set row, respectively. Wrong parent scope, missing indexes or target-only trust inclusion are the specific regression risks.

Validate the query directly through `person_edit_previews_bind_exact_action_specific_delta_counts`: overlapping branches, membership-only reachability, unrelated Look ownership, target assignment/suggestion inclusion, target-only constraint/trust exclusion and dangling Face references. Then run unchanged `whole_person_assignment_preview_streams_pages_and_surfaces_delta_bound` (4097 Faces/media), the existing 4096-suggestion same-media fixture, and `person_edit_delta_limit_allows_4096_and_blocks_4097_before_row_materialization`; verify schema-index creation and rerun the full suite after focused checks pass. Runtime results remain pending until these production-store tests execute.

</topic>

<topic id="red-team" status="active" version="2" wp="WP-085" summary="Sensitive over-export, hostile or partial imports, ambiguous reset scope, duplicate identities, unsafe original-file mutation, and matcher divergence are the hard failures." updated_at="2026-08-23">

## Risks and controls

- Oversharing: content preview; no embeddings/crops by default.
- Partial/newer import: version gate, transaction, rejected raw payload, rollback.
- Resource/path attack before dry-run: frozen byte/entity/string/nesting/semantic-reference/JSON-token-work ceilings, canonical-root enforcement, no traversal/symlinks/unsupported containers, and complete hash/schema/graph validation before writes.
- Duplicate/reconnected identities: stable IDs, idempotency, constraints, graph reconciliation.
- Flattened Looks or changed matcher trust: version and reconcile Look membership plus trusted-reference authorization/pins independently from regenerable embeddings.
- State promotion on import: version suggestion/strict-automatic/operator-confirmed explicitly, preserve automatic-generation ownership, and reject ambiguous promotion to durable evidence.
- Review semantics weaken across versions: encode Same/Different/Not sure as typed operations, map This is not to Different, and reconcile confirmation/no-change/cannot-link effects independently.
- Eligible face becomes trusted on restore: require independent explicit authorization and explicit Look membership in addition to all eligibility evidence; never infer membership from gate success.
- Original mutation: sidecars by default and explicit preview for any later opt-in write.
- Rebuild accidentally clears People: typed non-overlapping reset modes; destructive clear-all requires verified recovery and exact state-bound confirmation.
- Search divergence: extend indexed and reference paths together with large-fixture parity.

</topic>

<topic id="microtask-plan" status="active" version="2" wp="WP-085" summary="Complete search parity first, then versioned export, bounded safe import, optional interop, and two typed reset modes." updated_at="2026-08-23">

## Microtasks

1. Add person query grammar, matching, autocomplete, persistence, and receipts.
2. Define and validate the versioned Facial identity bundle including Unsorted/Looks, assignment/evidence states and generations, typed review operations, independent trusted-reference authorization plus eligibility evidence, frozen input ceilings, content hashes, and canonical container/path rules.
3. Implement export and content preview.
4. Implement bounded hostile-input parsing, graph/hash/schema validation, dry-run import, conflicts, relocation, transaction, rollback, and reconciliation with structural no-promotion checks for confirmation and trusted enrollment.
5. Add optional XMP sidecar import/export fixtures.
6. Implement `rebuild_match_analysis` and `clear_all_match_data` with non-overlapping affected-state manifests, verified pre-clear recovery, state-bound confirmation, exact durable-graph restore plus independent derived-state rebuild proof, and independent raw-media preservation proof.

</topic>

<topic id="takeover-validation" status="active" version="1" wp="WP-085" updated_at="2026-09-06" summary="Current source review found autocomplete gating and XMP projection boundary defects; prove recovery through the real inference worker.">

Rechecked the existing MWG schema reference (https://exiv2.org/tags-xmp-mwg-rs.html) and IPTC 2025.1 source on 2026-09-06. Keep the existing namespace-aware bounded XML parser and sidecar-only staging contract. Reject zero-area rectangles consistently with the manual-face consumer; explicitly report unsupported geometry/wrapper properties instead of silently dropping them. A parser replacement or silent geometry normalization would add scope and conceal the boundary mismatch, so neither is selected. Validate zero-width/height and unsupported-property fixtures, then the existing XMP staging/round-trip and hostile-input suite.

The person-autocomplete UI must query the existing stable-ID catalog independently of a completed membership search for the still-incomplete typed token. Validate actual UI orchestration, partial names/aliases and negative terms, then inspect the affected UI fixture. Recovery proof must run the actual provisioned inference worker against a hash-bound existing source-check image, verify persisted vectors before and after clear/restart/restore, independently reconcile durable operator graph rows, and prove source hashes unchanged. Synthetic inserted vectors remain component proof only.

</topic>
