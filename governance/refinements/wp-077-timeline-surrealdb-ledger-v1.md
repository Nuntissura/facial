---
file_id: REF-WP-077-TIMELINE-SURREALDB-LEDGER-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-077" summary="Build the deterministic timeline engine as a new parallel SurrealDB domain inside Facial, using the current K-pop vault only as an integration target and as preparation for a future media database migration." updated_at="2026-08-15">

## Operator direction

- Facial already has a database, but the timeline engine must receive its own parallel database.
- The operator intends to migrate the current Facial database to SurrealDB later; this ledger is the migration proving ground.
- Do not let workers author receipts or ingestable documents. The application alone creates every persisted record after validation.
- Keep the engine relocatable because Obsidian folders move. Handshake will ultimately replace the temporary vault workflow.

## Selected boundary

Add an embedded, file-backed SurrealDB ledger under the discovered timeline project's `.facial/timeline-ledger/` directory. It is a second database with a separate namespace, database, schema migrations, export, and test fixture. `media.redb` remains untouched.

Workers interact only through bounded `facial-cli timeline-ledger` proposal arguments. The application captures source bytes, canonicalizes URLs, derives hashes and IDs, and writes proposal state, receipts, rejections, canonical timeline facts, and projections. A malformed request creates at most an application-generated rejection audit; it never creates a worker-authored ingestable artifact.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-077" summary="SurrealDB's current Rust SDK supports embedded file-backed engines and logical export/import; Facial's Rust toolchain meets the SDK's stated minimum, so an isolated embedded ledger provides a real migration surface without touching redb." updated_at="2026-08-15">

## Verified external basis

- SurrealDB's Rust SDK documents embedded memory and file-backed engines as well as remote operation. The current SDK documentation states Rust 1.89 as its minimum; this workstation reports Rust 1.91.1. [Rust SDK](https://surrealdb.com/docs/reference/rust) and [embedded engine guide](https://surrealdb.com/docs/reference/rust/embedding).
- The Rust SDK supports logical export; SurrealDB documents SurrealQL exports as portable backups and recommends post-import sanity checks. [Rust export](https://surrealdb.com/docs/languages/rust/methods/export) and [backups and recovery](https://surrealdb.com/docs/manage/self-hosted/backups-and-recovery).
- SurrealKit is the official schema migration tool and uses committed SurrealQL schema files. [SurrealKit schema migration](https://surrealdb.com/docs/manage/schema-migration).
- Facial currently uses `redb = "4"` for media state. It stays live and unmodified, making WP-077 a bounded migration experiment rather than a risky rewrite.

## Rejected approach

- A second redb ledger does not exercise the planned SurrealDB migration path and is rejected.
- A remote or cloud database adds credentials, service lifecycle, and network failure modes to a local test target. Use embedded storage first; retain the same repository abstraction for a future remote endpoint.
- Direct worker JSON, Markdown, or file intake is rejected because an agent can malform it before validation.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-077" summary="Separate physical storage, schema migrations, logical exports, application-owned receipts, and anchor-relative resolution protect the live media store, evidence integrity, and folder mobility." updated_at="2026-08-15">

## Controls

- The SurrealDB ledger path is project-anchor relative and cannot resolve to the current media database.
- Schema changes use ordered SurrealQL migrations and record the applied version.
- Every migration and projection has an export/re-import equivalence test before it is considered durable.
- A raw capture and content hash are required before a source proposal can be promoted.
- Rejected submissions are application-generated diagnostics, not worker-generated records.

</topic>
