---
file_id: REF-WP-056-MEDIA-QUERY-STAT-RESPONSIVENESS-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-056" summary="Keep search, sorting, and metadata views responsive for very large local or NAS collections." updated_at="2026-08-09">

## Operator request

- Implement the approved large-library search, sorting, and metadata-pipeline
  improvements after WP-055 establishes generation-safe inventory state.
- Remove complete-collection query preparation, sorting, stat work, and cached-folder
  rebuilding from interactive render frames.
- Preserve the selected-folder search scope and existing Media behavior.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-056" summary="Current source inspection and network-filesystem guidance support immutable indexes and cancellable background computation." updated_at="2026-08-09">

## Sources and patterns checked

- Current Facial source inspection found query changes building normalized keys and
  ranking rows for the complete collection on the UI thread, final name sorting on
  scan completion, and Size/Modified sweeps issuing metadata calls for every path.
- Rust directory entries can reuse file-type and metadata captured during enumeration
  on Windows, avoiding additional calls where those facts are sufficient:
  <https://doc.rust-lang.org/std/fs/struct.DirEntry.html>
- Windows SMB 3.1.1 uses larger directory-query buffers and enhanced directory caching
  to reduce round trips for large directories:
  <https://learn.microsoft.com/en-us/windows-server/storage/file-server/smb-feature-descriptions>
- Field implementations such as PhotoPrism and Immich keep searchable/indexed state
  separate from original media reads and expose bounded worker controls for large
  libraries:
  <https://github.com/photoprism/photoprism>
  <https://github.com/immich-app/immich>

## Selected approach

- Build immutable normalized search rows when an inventory generation changes.
- Debounce and execute query ranking off-thread with generation and query IDs; publish
  results atomically only when both still match.
- Sort final inventory views off-thread and reject obsolete completions.
- Make Size/Modified sweeps bounded, cancellable, generation-aware, and reusable across
  sort changes; reuse enumeration facts when available.
- Cache prepared child-folder display entries and render only the visible slice without
  cloning the complete collection each frame.

## Rejected options

- Faster networking as the only fix: it does not remove O(collection) UI-thread work.
- One worker per query or sort without cancellation: rapid input would accumulate stale
  work and increase memory and I/O pressure.
- Treating metadata errors as zero-valued authoritative facts: unavailable files and
  transient NAS failures must remain distinguishable from real values.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-056" summary="Generation rejection, bounded queues, and failure-aware metadata prevent stale or misleading results." updated_at="2026-08-09">

## Risks, failures, and controls

- Rapid typing publishes an old query: bind every job to inventory generation and query
  ID, then reject mismatches at the consumer boundary.
- A folder change publishes old sort/stat results: cancel the old generation and reject
  any event whose root identity or generation is no longer active.
- Metadata failure silently reorders files: retain an explicit unknown/error state and
  use a deterministic fallback order.
- Background work grows without bound: use a coalescing single-latest request channel
  and bounded result batches.
- Prepared search rows duplicate excessive strings: store shared immutable path/name
  data and measure memory at the 141,400-item boundary.
- Folder virtualization still clones all rows: cache the complete immutable prepared
  entry set and allocate only the visible range during paint.

</topic>

<topic id="acceptance-plan" status="active" version="1" wp="WP-056" summary="Acceptance proves responsive interaction, stale-result rejection, and exact view semantics." updated_at="2026-08-09">

## Verification needs

- Unit tests for query generation, debounce/coalescing, stale result rejection,
  deterministic ordering, unknown metadata, cancellation, and root changes.
- Structured diagnostics proving no render frame performs complete-collection query,
  sorting, stat, or child-entry preparation.
- A 141,400-item synthetic fixture plus the exact NAS collection when mounted.
- Rapid query, sort, and folder-switch probes while scan and playback are active.
- Full-suite and adversarial producer/consumer review before status completion.

</topic>
