---
file_id: REF-WP-084-MATCH-CORRECTIONS-MANUAL-FACES-UNDO-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="active" version="1" wp="WP-084" summary="Make Match quietly visible in Viewer metadata and make every face assignment explicitly editable, durable, understandable, and reversible without intruding on normal browsing." updated_at="2026-08-22">

## Product contract

The normal Viewer shows only a compact People row above labels/tags/notes when committed assignments exist. When none exist, the row is absent and one low-emphasis `Faces` action in the existing file-identity row keeps manual editing discoverable after Match is configured. No boxes, unknown-face prompts, or suggestions appear during ordinary browsing. `Edit faces` explicitly enters a transient overlay mode with stable-FaceId boxes, a keyboard-accessible face list, and manual-selection Person autocomplete; navigation, Settings, Escape, or immersive entry discards uncommitted UI state and closes Edit faces. Fullscreen exit restores the ordinary Viewer with Edit faces closed and never resurrects a discarded draft or editor; only the fullscreen hold and resulting execution eligibility are restored according to the remaining reasons and persisted operator mode.

Users can assign/reassign a face, mark `This is not <name>`, defer with `Not sure`, ignore a real face, invalidate `Not a face`, create a missed region, merge duplicate people, split contaminated groups, and undo. Reviewing a suggestion or strict-automatic assignment uses exact typed transitions: `Same` creates `operator_confirmed` Person evidence but does not authorize trusted enrollment; `Different` rejects or removes the candidate assignment and adds a face-to-candidate-Person cannot-link; `Not sure` changes neither identity truth nor constraints. `This is not <name>` invokes the same `Different` transaction for the currently named Person. A `suggestion` remains unassigned, and `committed_strict_automatic` remains model-derived and generation-bound until explicit confirmation.

The same semantics work on one Viewer face and on a virtualized canonical selection inside Match -> People. Single-face corrections apply with Undo; batch correction/removal, merge/split, and Person removal preview exact affected-object counts first. A correction is durable user truth and cannot be silently reversed by reindexing or a model upgrade. Normal autocomplete assigns only the Person and deterministically places the observation in `Unsorted`; an existing-Look move or `Same person, new look` is explicit. Person confirmation, Look membership, and trusted matcher enrollment are separate. TrustedTemplateSet enrollment requires independent explicit trusted-reference authorization, explicit Look membership, and all alignment, quality, generation, pose/diversity, and provenance gates; passing gates never auto-enrolls an observation.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-084" summary="Competitor gaps center on detector-dependent recovery and irreversible/opaque merges; stable IDs, hard constraints, and operation deltas provide a stronger contract." updated_at="2026-08-22">

## Sources and selected approach

- Google correction/merge behavior: https://support.google.com/photos/answer/6128838
- Apple Photos Show/Hide Face Names, inline autocomplete, manual regions, and This is Not: https://support.apple.com/en-nz/guide/photos/phtad9d981ab/mac
- Immich manual face controller and reassignment patterns: https://github.com/immich-app/immich/blob/main/server/src/controllers/face.controller.ts
- W3C manual-selection combobox interaction: https://www.w3.org/WAI/ARIA/apg/patterns/combobox/
- Microsoft region schema: https://learn.microsoft.com/en-us/windows/win32/wic/-wic-people-tagging

Use normalized regions plus orientation/source dimensions. A manual rectangle runs the existing landmark engine; invalid alignment may be assigned manually but emits no propagated embedding. Autocomplete resolves stable Person IDs, permits duplicate display names with cover/alias context, and never silently merges or commits its first suggestion. Person-only assignment enters `Unsorted`; Look placement is a separate explicit move. Merge/split use stable people and typed deltas/relations, not destructive deletion or a universal event-sourcing framework.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-084" summary="Intrusive overlays, stale workers, poisoned references, stale undo, indirect reclustering, and ambiguous correction language can destroy usability or trust." updated_at="2026-08-22">

## Risks and controls

- Old undo erases later faces: delta/relationship semantics preserve later independent assignments.
- Different pair reconnects indirectly: face-to-candidate-person cannot-link enforced in all paths.
- Bad box teaches the model: landmark gate and manual-only assignment when invalid.
- Confirmed mistag teaches the model: identity-pool membership remains separate from quality/Look/diversity-gated trusted references.
- Eligible observation auto-enrolls: require independent explicit trusted-reference authorization plus explicit Look membership and every eligibility gate; gate success never writes membership.
- Automatic assignment masquerades as confirmation: keep `committed_strict_automatic` model-derived and generation-bound until `Same` or explicit manual assignment.
- Ambiguous review language corrupts truth: type `Same`, `Different`, and `Not sure`; map `This is not <name>` exactly to `Different`.
- Stale worker restores a correction: Person/Face revision fencing rejects old recognition writes.
- Boxes dominate the photo: overlays are opt-in and transient; ordinary Viewer shows only a bounded People summary.
- Duplicate names select the wrong record: autocomplete operates on stable Person IDs and shows cover/alias context.
- Delete person deletes assets: structural route guard and explicit affected-object preview.
- Not sure becomes negative evidence: defer-only state with no training/constraint effect.
- Fullscreen exit revives stale edits: immersive entry discards drafts and closes Edit faces; exit restores only ordinary Viewer presentation and reason-aware execution eligibility.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-084" summary="Implement correction semantics and invariants before the visual tools that invoke them." updated_at="2026-08-22">

## Microtasks

1. Define Viewer People-row, overlay lifecycle, stable-FaceId editor, explicit assignment/evidence states, Unsorted/Look membership, correction, review, operation, undo, deletion, and one-way immersive draft-discard semantics.
2. Implement assign/reassign/remove/ignore and exact Same/Different/Not sure transactions, including the This is not mapping and separation from trusted authorization.
3. Implement merge/split previews, apply, persistent undo, and conflict handling.
4. Implement orientation-safe manual regions and landmark/embedding gates.
5. Add the compact Viewer People row, explicit overlays, Person autocomplete, Unsorted and Looks shortcuts, receipts, diagnostics, and inspector fixtures.
6. Prove restart/reindex/model-change round trips, state/Look/trust transitions, fullscreen entry-discard/exit-closed behavior, pause interlocks, stale-worker rejection, overlay-off performance, and source-level deletion guards.

</topic>
