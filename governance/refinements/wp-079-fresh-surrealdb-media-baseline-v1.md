---
file_id: REF-WP-079-FRESH-SURREALDB-MEDIA-BASELINE-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="completed" version="1" wp="WP-079" summary="Start the media application database fresh because legacy curation data has no retained operator value; keep Match blocked until the clean baseline and legacy retirement are proven." updated_at="2026-08-22">

## Operator request

The operator does not care about legacy tags, notes, or labels and accepts a fresh start when it is cleaner and faster. The fresh start also discards legacy favorites, settings, inventory, and CLIP cache rows unless the operator changes scope before execution. Keep one untouched cold backup initially so this waiver is recoverable. This decision applies only to the legacy **media** database; it does not authorize any Timeline-ledger loss.

## Spec anchors and scope edges

- Current application store: `specs/app-spec.md` section 15, WP-042/WP-078 media database.
- Current runtime topology: `topology.yaml` `media_browser.state_paths.media_db`.
- Preserve raw media, unrelated databases, and the separate timeline store.
- Do not add a temporary legacy database runtime to the shipped app.

</topic>

<topic id="research-basis" status="completed" version="1" wp="WP-079" summary="The current Rust app already resolves only SurrealDB while the waived legacy media values are either unwanted or regenerable, making a clean baseline lower-risk than a conversion pipeline." updated_at="2026-08-22">

## Evidence and selected approach

- `product/Cargo.toml` resolves embedded SurrealDB and no `redb` dependency.
- `product/src/media_db.rs` owns notes, tags, labels, favorites, settings, and inventory; `product/src/media_clip.rs` owns regenerable CLIP embeddings.
- WP-078 already established the SurrealDB application-root contract but its broader Timeline data remains valuable and separately located.
- Selected approach: classify and content-hash exact targets, reconcile every present and absent candidate after each long scan, create and content-verify the cold copy, quarantine the current engine marker before its SurrealDB directory, reconcile all exact targets plus protected state and emit the ready manifest, then initialize and prove the replacement clean root while retaining both recovery copies outside live discovery.
- Scalability correction from the live preflight: the protected raw-workspace tree is 571,754,768,872 bytes across 2,422,993 files. A content-hash Audit read only 1,936,685,421 bytes after about three hours and would have repeated the full read during Execute. Protected raw-workspace, thumbnail-cache, and unrelated-state proof therefore uses a compact deterministic metadata-tree digest whose directory records contain relative path/type and whose file records additionally contain size, UTC creation/write ticks, and attributes; exact retirement targets, both recovery copies, and anchored Timeline artifacts retain byte-content SHA-256 proof. Directory timestamps are deliberately excluded because creating the excluded retirement root and moving excluded targets legitimately changes ancestor-directory timestamps.
- Rejected: lossless import, because it adds a temporary reader, conflict semantics, reconciliation, and deletion risk for data the operator waived.
- Rejected: hashing every protected raw-media byte and emitting millions of per-file manifest rows, because it turns a bounded database retirement into multi-day I/O and unbounded manifest/memory growth without strengthening the exact-path mutation boundary.

</topic>

<topic id="red-team" status="completed" version="1" wp="WP-079" summary="The primary risks are confusing the media database with raw media, another database, or the Timeline ledger, and making the waiver irreversible too early." updated_at="2026-08-22">

## Risks and minimum controls

- Misclassified target: canonicalize, inspect signatures/tables, hash, and stop on ambiguity.
- Accidental Timeline loss: when a Timeline ledger exists, anchor and independently reconcile it before and after; when none exists, require the explicit `-NoTimelineLedger` acknowledgement in both Audit and Execute and record that proof in the manifest.
- Regret after fresh start: retain an untouched cold backup; permanent deletion requires a later exact target approval.
- Hidden fallback: prove separate-process path resolution and guard against legacy live discovery.
- False completion from an empty UI: write/read new values, rebuild inventory, optionally rebuild CLIP, restart, relocate, and re-read.
- Protected-tree drift or a concurrent writer: bind compact metadata-tree digests into Audit approval, recompute them before any move, reconcile them after the exact move, and fail closed on path/type/count/size/time/attribute digest drift or any reparse point.
- Exact-target TOCTOU or racing startup: reconcile all seven present/absent target paths after every long protected scan and immediately before ready; move the engine marker before the database so Facial cannot recreate the store between renames; retain deterministic target-appearance and rollback regression probes.
- Multi-day safety tooling: prohibit protected-file content reads and per-file protected manifest rows; performance proof must cover a high-cardinality fixture and the live 2.42-million-file tree.

</topic>

<topic id="microtask-plan" status="completed" version="1" wp="WP-079" summary="Audit, preserve, quarantine, reconcile, initialize, and prove in that order." updated_at="2026-08-22">

## Microtasks

1. Audit exact source/target ownership and content hashes plus compact protected metadata-tree digests, then bind approval to the observed live state.
2. Create and independently verify the untouched cold backup without changing live discovery.
3. Quarantine the audited current live targets marker-first, content-verify both recovery copies, reconcile all exact target paths and every compact protected metadata-tree inventory, and emit `ready-for-clean-initialization` only while all seven exact candidates are absent without deleting either recovery copy.
4. Initialize the replacement schema-marked SurrealDB media root only after that ready manifest exists.
5. Prove clean counts, new writes, separate-process restart, relocation, inventory regeneration, and optional CLIP regeneration while retaining both recovery copies outside live discovery.
6. Update runtime/spec/topology/Manual evidence only after proof.

</topic>

<topic id="completion-evidence" status="completed" version="1" wp="WP-079" summary="The live clean SurrealDB baseline, retained legacy recovery copies, restart and relocation behavior, GUI path, full Rust suite, and packaged 0.1.8 verifier all passed their independent proof gates." updated_at="2026-08-22">

## Completion evidence

- Bound retirement manifest: `.facial-media-retirement/wp-079-fresh-baseline-20260822/manifest.json`, SHA-256 `DDBA52FF6C013BA97994587909846994E7BAB608CAF756D52BD4B0C5C8890265`, status `ready-for-clean-initialization`, `source_deletion=never`.
- Exact legacy targets, cold backup, and quarantine copies are content-SHA-256 equivalent. Both recovery copies remain outside live discovery and the installer `.facial` cleanup boundary.
- Two separate hidden CLI processes reported the same schema-marked SurrealDB store: `clean_user_state=true`, two internal settings, and zero user, inventory, staging, or CLIP rows. Restart and whole-workspace relocation probes passed.
- The Rust suite completed with 304 passed, zero failed, and five ignored tests; `cargo tree` contained no `redb` package.
- A background-safe live Media snapshot was directly inspected and showed the thumbnail grid plus readable lower-right labels, tags, and notes with no foreground activation. First row was 240 ms, first thumbnail 642 ms, and settled state 1,291 ms.
- Independent Windows PowerShell 5.1 and PowerShell 7 checks accepted the final 0.1.8 portable and setup artifacts with zero violations. No real install, update, uninstall, or live 0.1.7 predecessor transition was performed.

</topic>
