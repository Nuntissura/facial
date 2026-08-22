---
file_id: REF-WP-087-MATCH-INTEGRATION-HARDENING-RELEASE-V1
file_kind: refinement
updated_at: "2026-08-22"
---

<topic id="operator-request" status="active" version="1" wp="WP-087" summary="Ship Match only after independent proof that the complete module is trustworthy, model-operable, recoverable, responsive, and packaged correctly." updated_at="2026-08-22">

## Closure contract

This packet integrates and independently verifies WP-080 through WP-086. It cannot soften or replace their gates. Match is complete only when the packaged app proves secure model loading, strict identity behavior, durable corrections, safe reset, recovery, visual usability, background operation, and fresh-context model control.

Release also requires proof that Match preserves Facial's visual-first, large-folder browsing contract. Normal Media browsing is quiet and pull-driven; only committed manual or strict-automatic assignments may appear in the compact metadata row with provenance, while every editor, face box, suggestion, prompt, seek, and review surface requires an explicit action. Playback and immersive fullscreen are attributable pause reasons, and fullscreen removes every Match presentation in its first frame without changing the persisted operator mode.

The reproducible pre-existing Media multi-label relative-p95 failure is a hard predecessor, not a Match exception. Match release remains blocked until an independent reviewer either proves the canonical gate resolved or attributes the failure as pre-existing from a same-binary Match-unavailable baseline with raw comparable evidence. Match work may not hide, waive, suppress, loosen, or relabel that gate.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-087" summary="Facial's existing build rules require shared rendering, structured routes, direct visual inspection, full failure paths, independent adversarial review, and packaged-runtime proof." updated_at="2026-08-22">

## Authority and reuse

- `CODEX.md` sections 6, 7.1, 7.2, 8, and 8.3.
- `governance/build_rules.yaml` model operation, GUI, diagnostics, persistence, review, and packaging gates.
- Existing `ui-inspect`, `ui_snapshot`, receipts, Manual, release script, and executable-layout validator.
- Existing Media frame diagnostics, Viewer metadata-band contract, chrome-hidden state, native-video placement/capture diagnostics, virtualized viewports, and background-safe intent routes.

Selected approach: assemble a complete state/failure matrix, fill missing intents/diagnostics/fixtures, run exact large-library and recovery drills, independently review high-risk producer-consumer boundaries, then package and re-prove the same contract from extracted artifacts.

Presentation proof uses generated media and fictitious People in deterministic fixtures. Exact-live state is inspected only through the existing background-safe route and must be redacted by default when it would expose operator identity material; an explicit bounded privacy-marked capture is the only exception. Frame proof compares Match disabled, active-but-quiet, and explicit editing rather than accepting a fast empty fixture. Match disabled means unavailable or unconfigured with zero Match worker, model-load, or index-query admission; operator-paused is measured separately and cannot stand in for disabled.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-087" summary="Happy-path polish, sensitive diagnostics, incomplete resets, async misattribution, and source-only proof can create a false release." updated_at="2026-08-22">

## Risks and controls

- Happy path hides failure corruption: inject model, DB, cancellation, restart, import, undo, reset, and package failures.
- Face data leaks into diagnostics: redaction guards and bounded explicit opt-in payloads.
- Reset leaves vectors/crops: exact table/cache manifest and independent absence check.
- Old job changes new state: generation/request attribution and ABA tests.
- Source tree works but package fails: extracted portable/setup runtime probes are mandatory.
- Self-review misses systemic risk: independent adversarial review with no unresolved high-risk finding.
- Quiet engine, noisy product: forbid spontaneous boxes/prompts/toasts/focus changes and inspect quiet success, quiet failure, and quiet queued-work states directly.
- Fullscreen clears an operator pause: reason-set interleaving tests and receipts separate persisted mode from transient holds.
- Fullscreen exit revives a discarded face editor: return to ordinary Viewer with Edit faces closed and assert drafts, boxes, selection, and autocomplete state are absent.
- Dense faces create unreadable overlays: show detail only for selected/hovered geometry and retain a keyboard-accessible, virtualized metadata list.
- Large People catalog blocks paint: virtualize rows/results, cache projections, and guard against database/filesystem/model/count/worker work in render.
- Settings duplicates or eagerly prepares the People manager: render zero People rows/counts/covers in Settings and measure its Manage people route separately from the Match -> People destination open.
- A paused engine is used as the disabled A/B baseline: assert unavailable/unconfigured state and zero worker/model/index-query admission.
- Benchmark parameters drift between runs: one versioned protocol/result artifact fixes the build, hardware/display manifest, warmup, samples, workloads, percentile method, and A/B invariants.
- Shared filesystem permits pass while Match exhausts another resource: record exact WP-081 item/byte/concurrency ceilings, jointly saturate CPU/inference, decoded memory, optional GPU/VRAM, SurrealDB writes/index builds, and queues, and require visible-work budgets plus zero terminal lease leaks.
- The known Media multi-label p95 failure is hidden by Match release reporting: require an independent resolved-or-attributed predecessor verdict with raw baseline proof and forbid threshold/fixture manipulation.
- Correction labels drift from stored semantics: assert one mapping in UI, receipt, operation log, and constraints for This is not/Different, Same confirmation, and Not sure defer.
- Visual evidence leaks operator identities: synthetic fixtures, default redaction, seeded canary scans, and explicit bounded privacy-marked exact-live capture.
- Native video covers or desynchronizes a face editor: accept metadata track/timestamp editing or a paused egui-owned captured still only after the native child withdraws.

</topic>

<topic id="presentation-proof-matrix" status="active" version="1" wp="WP-087" summary="Release proof covers quiet browsing, opt-in correction, dense faces, large People, fullscreen, focus safety, native video, frame time, and privacy without operator-data leakage." updated_at="2026-08-22">

## Required deterministic fixtures

- Quiet Media with no committed assignment: no Match row, box, unresolved count, badge, prompt, toast, or automatic editor; the low-emphasis Faces entry action may remain after Match configuration.
- Quiet Media with committed manual or strict-automatic assignments: only the compact People row with provenance is present in the existing metadata band; no box/editor appears until explicit edit.
- Single-face and dense overlapping-face edit: normalized geometry remains selectable, only selected/hovered detail labels the image, and a keyboard-accessible metadata list provides the same exact actions.
- Correction vocabulary: visible **This is not <name>** performs **Different** semantics and persists the old-Person cannot-link; **Same** is an explicit review action that changes a suggestion or strict-automatic assignment to operator-confirmed truth; **Not sure** only defers and adds no negative evidence.
- Compact/high-font Viewer, a pathological 1,000-face asset, a synthetic 10,000-People catalog/autocomplete, and a 1M-face precomputed-count projection: controls remain readable, only selected/hovered face detail is laid out, rows stay virtualized/bounded, and rendering never materializes every assignment.
- Immersive fullscreen entered from running and operator-paused states: the first frame contains no Match presentation, effective-state receipts retain the exact reason set, and exit returns to ordinary Viewer with Edit faces closed without restoring discarded drafts, boxes, selections, or autocomplete state.
- Video timestamp/track correction: active native playback has no egui Match overlay; correction uses metadata rows or a paused captured-still surface with exact asset/track/timestamp/geometry correspondence.
- Empty, indexing, partial, failed, cancelled, stale-generation, reset-preview, and recovery states: none produces an unsolicited prompt or focus change.

## Performance and interruption gates

- Compare Match disabled, active-but-quiet, and typical explicit edit on the recorded reference hardware. Disabled means unavailable or unconfigured with no Match workers, model loads, or index queries admitted; it is not operator-paused. Across rolling two-second frame windows sampled every 250 ms with at least 60 frames, normal and typical edit rendering must each keep worst-window p95 at or below 16.7 ms; an undersampled window fails the run.
- Keep worst-window p99 at or below 33.3 ms; keep the 1,000-face edit worst-window p95 at or below 33.3 ms; keep cached autocomplete at or below 100 ms p95 and never above 200 ms; open the virtualized 10,000-People manager within 200 ms p95; render 1M-face counts/covers from precomputed projection only.
- The 10,000-People fixture belongs to virtualized Match -> People. Settings -> Match renders no People rows/counts/covers; its Manage people route acknowledges input within 100 ms p95 before the separate destination-open measurement.
- Pause, route, and autocomplete state must be visible within 100 ms p95. No new Match stage may begin later than 250 ms after an operator or immersive hold; every admitted safe unit checkpoints or ends within the WP-086 hard 2,000 ms limit, otherwise it times out and quarantines its isolated worker/generation.
- Source and runtime guards prove zero database, filesystem, model, count-rebuild, or worker-start work from the paint path.
- The benchmark records configured ceilings and observed peaks for admitted items, aggregate queue items/bytes, CPU/inference concurrency, decoded image/crop bytes, optional GPU/VRAM bytes, SurrealDB write concurrency, and vector-index-build concurrency. Joint saturation with remote I/O, navigation, visible thumbnails, and Viewer playback must backpressure without exceeding a ceiling, leaking a lease, or failing an existing visible-work budget.
- Match completion, failure, suggestion, and correction never activate/raise the app, capture focus, open a modal, change the selected media/tab, seek video, or start analysis without an explicit action.

## Versioned benchmark protocol/result artifact

The one canonical artifact is `governance/validation/wp-087-match-benchmark-v1.yaml`, schema version 1. It contains both the immutable protocol fields and the measured result/verdict fields; parallel prose or screenshots cannot replace it.

- Build: Cargo `release` profile measured from the version-matched packaged portable executable. Record git commit, app version, `Cargo.lock` SHA-256, model generation, and schema generation.
- Reference hardware: record CPU model and physical/logical cores, RAM bytes, GPU and driver, inference backend, Windows edition/build, power mode, storage kind, media-root kind, and network link.
- Display: 1920x1080 physical pixels, 100-percent DPI, egui pixels-per-point 1.0, Inter at 19 pt. A different display profile is a separate non-comparable result, not a substitute gate.
- Render sampling: discard a 30-second warmup, then measure 120 seconds and at least 7,200 raw frame records per state, each containing monotonic frame-end timestamp and duration. For measurement interval `[T0,T1)`, every required rolling window starts at `T0 + k*250ms` for nonnegative `k` where `start + 2000ms <= T1`; a record belongs to the half-open window when its frame-end timestamp is in `[start,start+2000ms)`. Evaluate every required start, require at least 60 frames in each window, and fail the run if any window is missing or undersampled. Interaction endpoints discard 20 warmup calls and record 200 calls. Concurrent indexing/playback discards 60 seconds and measures 600 seconds.
- Workloads/endpoints: Match unavailable/unconfigured, active quiet, typical face edit, pathological 1,000-face edit, Match -> People 10,000 open, Settings Manage people route feedback, autocomplete, pause/hold admission and checkpoint, concurrent indexing/playback, and the Media multi-label predecessor.
- Resource-governor evidence: record every configured ceiling, observed peak, backpressure event, terminal lease balance, and navigation/thumbnail/playback budget verdict while remote filesystem, inference, decode/crop memory, optional GPU, SurrealDB writes, vector-index builds, and inter-stage queues are saturated together.
- Percentiles: use a monotonic wall clock in microseconds and nearest rank over sorted frame durations, index `ceil(p*N)-1` with zero-based indexing. Do not trim or discard measured outliers. Record measurement bounds, whole-run N/p50/p95/p99/max, and a SHA-256 of timestamped raw sample records, plus window parameters, required/evaluated window counts, the worst rolling-window p50/p95/p99/max and exact time bounds, and an explicit rolling verdict. Whole-run percentiles are diagnostic and cannot replace the worst-window gate.
- Comparability: run the render A/B in `disabled, active quiet, active quiet, disabled` order and require identical packaged binary, hardware manifest, display profile, fixture generation, cache state, input script, and power mode. Any mismatch invalidates the comparison.
- Comparable gates: normal/typical-edit worst rolling-window p95 <=16.7 ms; worst rolling-window frame p99 <=33.3 ms; 1,000-face worst rolling-window p95 <=33.3 ms; every required rolling window has at least 60 frames; pause/route/autocomplete p95 <=100 ms; autocomplete max <=200 ms; Match -> People 10,000 open p95 <=200 ms; no new stage after 250 ms; in-flight safe-unit hard limit 2,000 ms.

## Media multi-label hard predecessor

The artifact records one independently reviewed predecessor verdict. `resolved_pass` requires the canonical Media inspector and its multi-label relative/absolute p95 gate to exit zero under this reference protocol. `independently_attributed_preexisting` requires the same packaged binary with Match unavailable to reproduce the failure, raw samples to isolate a non-Match cause, and an independent reviewer to prove Match adds no regression under the corrected comparable protocol. Red, unknown, self-attributed, hidden, waived, suppressed, threshold-loosened, fixture-swapped, or relabeled outcomes block Match release.

## Privacy evidence

- Deterministic fixtures use generated media and fictitious People only.
- Seed canary names, crop identifiers, region coordinates, vector tokens, and similarity values, then scan ordinary logs, receipts, crash output, layout JSON, app-generated captures, snapshots, and packaged diagnostics for zero disclosure.
- Default diagnostic and model-capture paths redact sensitive Match presentation. A sensitive exact-live capture is explicit, limited to the current requested surface, privacy-marked, and never used as an ordinary background artifact.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-087" summary="Complete the state matrix, fill structured tooling gaps, prove failure/recovery and visuals, then package and independently audit." updated_at="2026-08-22">

## Microtasks

1. Build the full feature/state/failure/acceptance matrix from WP-080 through WP-086.
2. Add the quiet, committed-assignment-row, single/dense-face, correction-vocabulary, compact/high-font, virtualized Match -> People 10,000, Settings-route-with-zero-People-rows, fullscreen-editor-closed, video-correction, failure, and privacy fixtures through the shared render path.
3. Add missing pause/focus/privacy intents, diagnostics, snapshots, state fields, and Manual instructions.
4. Run focused/full tests, correction-semantic and pause/fullscreen interleavings, the v1 A/B benchmark, prohibited-paint-work and resource-governor saturation/lease probes, the Media multi-label predecessor audit, scale/starvation probes, privacy/reset canary scans, and recovery drills.
5. Inspect all deterministic states directly and run only bounded privacy-safe exact-live proofs without foreground activation.
6. Run independent adversarial review and resolve every high-risk finding.
7. Package, extract, re-prove Match including the presentation/privacy matrix, then synchronize spec/topology/taskboard/packet status.

</topic>
