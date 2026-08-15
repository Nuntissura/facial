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

Workers interact only through bounded `facial-cli timeline-ledger` proposal arguments. The application captures source bytes, canonicalizes URLs, derives hashes and IDs, and writes proposal state and rejection audits. Canonical timeline facts and Obsidian projections remain coordinator-owned until their application-owned migration path is separately implemented and proven. A malformed request creates at most an application-generated rejection audit; it never creates a worker-authored ingestable artifact.

</topic>

<topic id="timeline-gui" status="active" version="1" wp="WP-077" summary="Make Timeline a top-level Facial module with group/member navigation, event cards, bounded detail views, and deterministic visual inspection." updated_at="2026-08-15">

## Operator direction

- Timeline is a top-level Facial tab, not a hidden CLI-only tool.
- Show groups, members, fetched media links, canonical events, planned events, and evidence state.
- Present chronology professionally with collapsible event cards.
- Expanded events expose detail sections, including independently scrollable fetched media.
- Prefer tabs and subtabs where they preserve context.

## Selected information architecture

- Use one top-level `Timeline` app tab.
- Use a stable left rail for group/member selection because groups and members are navigation entities, not peer content panes.
- Use five short view tabs — `Overview`, `Events`, `Planned`, `Sources`, `Coverage` — for related views of the selected subject.
- Use chronological cards with a persistent date/status/title summary. Expansion exposes `Summary`, `People`, `Media`, and `Evidence` as a compact detail switcher; the Media pane owns a bounded scroll area so one media-rich event cannot displace the rest of the timeline.
- Label occurrence, publication, scheduled, and observed timestamps explicitly. Unknown location/time remains visibly unknown.
- Keep canonical sources and intake-only captures visually distinct. A capture proposal cannot appear as a verified event.

## Research basis

- DWP's production timeline guidance treats timestamp and title as the stable scan line, recommends matching displayed precision to available evidence, and warns against inventing a time when only a date exists: https://design-system.dwp.gov.uk/components/timeline/how-it-works
- Telerik's current timeline guidance recommends collapsible cards when event content includes substantial details or media, so later events remain visible: https://www.telerik.com/design-system/docs/components/timeline/usage/
- NICE's tab guidance limits a tab row to five, uses short labels, and reserves tabs for closely related content that does not need simultaneous comparison: https://design-system.nice.org.uk/components/tabs/
- Apple's current tab-bar guidance treats top-level tabs as persistent application hierarchy and recommends a sidebar when one area has deeper navigation: https://developer.apple.com/design/human-interface-guidelines/tab-bars
- W3C's card guidance keeps text first in source order and does not make a complex card one undifferentiated click target: https://design-system.w3.org/components/cards.html

## Red-team controls

- Timeline list scrolling and an expanded event's media scrolling use different stable scroll IDs.
- Expansion never hides the event's date, title, verification state, location state, or evidence count.
- Detail tabs never reinterpret source metadata as an occurrence fact.
- The left rail and card list remain independently scrollable so hundreds of events and future groups stay usable.
- A populated deterministic inspector fixture is mandatory; an empty state is not visual proof of the component.

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
