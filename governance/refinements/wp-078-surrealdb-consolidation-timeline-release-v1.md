---
file_id: REF-WP-078-SURREALDB-CONSOLIDATION-TIMELINE-RELEASE-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-078" summary="Make embedded SurrealDB Facial's only database technology, preserve the existing timeline ledger and captures, finish the Timeline GUI and IVE backfill, and publish the verified release." updated_at="2026-08-15">

## Operator request

- Remove the `redb` dependency and every Facial-owned `media.redb`, `inventory.redb`, and `clip_index.redb` file after proving each deletion target contains no raw media and belongs to Facial.
- Do not perform a lossless metadata conversion. Preserve images, videos, unrelated databases, the valuable timeline ledger, and all 152 captured sources.
- Make embedded SurrealDB operational for notes, tags, labels, favorites, settings, inventory, CLIP cache data, and the timeline ledger; prove metadata survives a separate-process restart before release.
- Align the K-pop timeline skill, synchronized templates, Claude mirror, and weekly automation with the relocated timeline project and SurrealDB ledger.
- Finish the Timeline GUI, perform the exhaustive IVE public-activity backfill through bounded parallel source lanes, validate the Obsidian projections, package the release, commit, and push.

## Supersession boundary

WP-078 supersedes WP-077's deliberately parallel database boundary. WP-077 remains the historical contract that introduced the timeline ledger and Timeline GUI; its GUI and ledger requirements are carried forward unchanged. The new operator request explicitly replaces every clause that retained or isolated the old media database.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-078" summary="Current SurrealDB 3 documentation supports embedded SurrealKV with per-commit sync, requires an explicit 2.x-to-3.x export path, and identifies unexpected-shutdown recovery as a live failure mode that needs restart proof." updated_at="2026-08-15">

## Sources checked

- Current Rust SDK documentation: https://docs.rs/surrealdb/latest/surrealdb/ and https://docs.rs/surrealdb/latest/surrealdb/struct.Connect.html
- Official 2.x-to-3.x migration guide: https://surrealdb.com/docs/build/migrating/from-old-surrealdb-versions/2x-to-3x
- Official schema migration guidance: https://surrealdb.com/docs/manage/schema-migration and https://surrealdb.com/docs/manage/schema-migration/rollouts
- Current SurrealDB issue tracker, including the open SurrealKV unexpected-shutdown report: https://github.com/surrealdb/surrealdb/issues/7426
- Current crate/release inspection showed `surrealdb` 3.2.4 as the newest stable SDK compatible with this build on 2026-08-15; the newer 3.3 line was prerelease-only.

## Patterns found and selected approach

- Use one shared embedded SurrealKV application store at `<workspace-root>/.facial/media/surrealdb`, with table/bucket separation for metadata, inventory, settings, and regenerable embeddings.
- Use `.sync("every")` because the SDK documents per-commit sync as the most durable mode and unexpected-shutdown recovery is an active field failure mode.
- Stamp every embedded store with an engine/schema marker and refuse unmarked non-empty or incompatible-major databases rather than guessing their format.
- Do not open SurrealDB 2.x files with the 3.x SDK. Read the preserved legacy database with the exact 2.x code, export neutral typed rows plus capture hashes, import into a fresh 3.x store, and independently re-read and compare every row and capture.
- Preserve workspace-relative media keys and anchor-relative timeline project discovery so whole-project relocation remains supported.
- Keep worker intake bounded to `timeline-ledger propose-source`; only the coordinator promotes canonical K-pop facts and vault projections.

## Reuse opportunities

- Reuse the existing media table contracts and tests through a narrow SurrealDB-backed key/value facade, minimizing unrelated UI/API churn.
- Reuse WP-077's source capture, proposal, rejection, doctor, and Timeline inspector surfaces.
- Reuse the K-pop skill's append-only registries, deterministic projection builder, strict validator, and required source-lane worker protocol.

## Rejected options

- Keeping `redb` for any application-owned table is rejected by the operator's sole-database requirement.
- In-place SurrealDB major-version opening is rejected because the official guide states 3.x cannot directly read 2.x data.
- Treating UI-visible counts, copied files, or an import receipt as migration proof is rejected; proof requires an independent canonical re-read and byte/hash comparison.
- Allowing workers to edit JSONL or Markdown directly is rejected by the timeline skill's single-writer and evidence-boundary rules.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-078" summary="Deletion ownership, restart durability, store shutdown, ledger equivalence, canonical-writer isolation, and deterministic GUI inspection are hard release controls." updated_at="2026-08-15">

## Risks and minimum controls

- Wrong-file deletion or raw-media loss: enumerate exact Facial-owned paths; open and inspect every database; reject unexpected tables or oversized/raw-media-like values; compare raw media and unrelated database inventories before and after deletion.
- Metadata appears to work only in-process: write note, tags, color/custom labels, and favorite in separate CLI processes, then verify all values from a fresh reader process.
- Embedded store shutdown races a same-process relocation/reopen: share handles per canonical path, retain a closing tombstone until the SurrealKV lock is released, and exercise move/reopen tests.
- Legacy ledger loss: preserve the old store, bundle, and captures; independently hash all 152 captures; compare all proposal/capture/rejection rows before and after import.
- Parallel workers corrupt canonical state: workers can only call the bounded ledger proposal command; the coordinator remains the sole append/projection writer.
- Timeline UI looks complete but hides or clips state: run every deterministic Timeline inspector preset, inspect the PNGs directly, and require layout/text gates to pass at full and compact sizes.
- Release contains a different executable or storage contract: package only through the canonical release script, independently extract and validate both executables, then rerun ledger and metadata probes from the packaged CLI.

</topic>

<topic id="validation-plan" status="active" version="1" wp="WP-078" summary="Completion requires independent storage, migration, restart, vault, GUI, full-suite, governance, package, and Git proof." updated_at="2026-08-15">

## Required proof

- `cargo tree` contains no `redb` package; source/current docs contain no live redb runtime claims.
- Focused media database, CLIP cache, timeline ledger, and store-shutdown tests pass; then the complete Rust suite passes.
- A separate-process metadata write/read cycle verifies note, tags, built-in/custom labels, and favorite after restart.
- The migrated ledger reports 152 proposals and its independent verifier matches all rows plus all 152 captures.
- All required K-pop source lanes are reconciled, canonical stores are append-only, projections rebuild, strict timeline and Obsidian Markdown validators pass, and the Obsidian topology lint passes.
- Every Timeline `ui-inspect` PNG/layout artifact passes the structured gates and direct visual inspection.
- The packaged portable/setup pair passes the independent executable-layout validator and packaged-runtime probes.
- An independent adversarial reviewer reports no unresolved high-risk finding.
- Intended changes are committed and pushed; the remote commit equals the local commit.

</topic>
