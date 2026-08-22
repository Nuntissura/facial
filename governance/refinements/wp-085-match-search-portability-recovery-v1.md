---
file_id: REF-WP-085-MATCH-SEARCH-PORTABILITY-RECOVERY-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="active" version="1" wp="WP-085" summary="Make Match identities searchable, portable, restorable, relocatable, and safely removable without coupling them to original files." updated_at="2026-08-22">

## Scope contract

Add `person:` and negated person terms without changing the existing all-terms-AND search grammar, reusing the stable-ID Person/alias autocomplete catalog from Viewer editing. Provide a versioned Facial identity bundle as the authoritative exchange format, including Looks, `Unsorted`, exact assignment/evidence states and generation provenance, typed review transitions, and independent durable trusted-reference authorization while excluding regenerable vectors. XMP is an optional previewed interoperability projection, sidecar-only by default.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-085" summary="Stable-ID JSON exchange preserves Facial semantics while IPTC/MWG regions provide limited interop with established photo tools." updated_at="2026-08-22">

## Sources and selected approach

- IPTC Photo Metadata 2025.1: https://www.iptc.org/std/photometadata/specification/IPTC-PhotoMetadata-2025.1.html
- MWG region tags: https://exiv2.org/tags-xmp-mwg-rs.html
- PhotoPrism XMP behavior: https://docs.photoprism.app/developer-guide/metadata/xmp/

Export durable identity data and manifests, including Looks, explicit Look memberships, `Unsorted`, trusted-reference authorization/pins and eligibility evidence, and the distinct `suggestion`, `committed_strict_automatic`, and `operator_confirmed` states while excluding embeddings/crops by default. Suggestions remain unassigned; strict-automatic assignments retain their model/calibration generation and model-derived ownership; import cannot promote either state to operator-confirmed evidence. Person-only assignments remain `Unsorted` until an explicit existing-Look move or `Same person, new look` operation.

Version review operations with exact semantics: `Same` creates operator-confirmed Person evidence without trusted authorization; `Different` rejects/removes and adds the face-to-candidate-Person cannot-link; `Not sure` changes neither identity truth nor constraints; `This is not <name>` is the `Different` transaction. TrustedTemplateSet restoration requires independent explicit authorization, explicit Look membership, and reconciled alignment, quality, generation, pose/diversity, and provenance evidence; eligibility alone never restores or creates membership. Import proceeds through dry-run, stable IDs, transactional application, conflict records, relocation mapping, rollback, and independent reconciliation. Preserve existing indexed/reference search parity and no-OR contract.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-085" summary="Sensitive over-export, partial imports, duplicate identities, unsafe original-file mutation, and matcher divergence are the hard failures." updated_at="2026-08-22">

## Risks and controls

- Oversharing: content preview; no embeddings/crops by default.
- Partial/newer import: version gate, transaction, rejected raw payload, rollback.
- Duplicate/reconnected identities: stable IDs, idempotency, constraints, graph reconciliation.
- Flattened Looks or changed matcher trust: version and reconcile Look membership plus trusted-reference authorization/pins independently from regenerable embeddings.
- State promotion on import: version suggestion/strict-automatic/operator-confirmed explicitly, preserve automatic-generation ownership, and reject ambiguous promotion to durable evidence.
- Review semantics weaken across versions: encode Same/Different/Not sure as typed operations, map This is not to Different, and reconcile confirmation/no-change/cannot-link effects independently.
- Eligible face becomes trusted on restore: require independent explicit authorization and explicit Look membership in addition to all eligibility evidence; never infer membership from gate success.
- Original mutation: sidecars by default and explicit preview for any later opt-in write.
- Search divergence: extend indexed and reference paths together with large-fixture parity.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-085" summary="Complete search parity first, then versioned export, safe import, optional interop, and reset." updated_at="2026-08-22">

## Microtasks

1. Add person query grammar, matching, autocomplete, persistence, and receipts.
2. Define and validate the versioned Facial identity bundle including Unsorted/Looks, assignment/evidence states and generations, typed review operations, and independent trusted-reference authorization plus eligibility evidence.
3. Implement export and content preview.
4. Implement dry-run import, conflicts, relocation, transaction, rollback, and reconciliation with structural no-promotion checks for confirmation and trusted enrollment.
5. Add optional XMP sidecar import/export fixtures.
6. Add reset previews and independent raw-media preservation proof.

</topic>
