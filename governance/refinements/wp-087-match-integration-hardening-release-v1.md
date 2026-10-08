---
file_id: REF-WP-087-MATCH-INTEGRATION-HARDENING-RELEASE-V1
file_kind: refinement
updated_at: "2026-10-08"
---

<topic id="operator-request" status="active" version="2" wp="WP-087" summary="Ship Match only after independent proof that the complete module is trustworthy, model-operable, recoverable, responsive, privacy-exact, and packaged correctly." updated_at="2026-08-23">

## Closure contract

This packet integrates and independently verifies WP-080 through WP-086. It cannot soften or replace their gates. Match is complete only when the packaged app proves secure model loading, strict identity behavior, durable corrections, safe reset, recovery, visual usability, background operation, and fresh-context model control.

Release also requires proof that Match preserves Facial's visual-first, large-folder browsing contract. Normal Media browsing is quiet and pull-driven; only committed manual or strict-automatic assignments may appear in the compact metadata row with provenance, while every editor, face box, suggestion, prompt, seek, and review surface requires an explicit action. Playback and immersive fullscreen are attributable pause reasons, and fullscreen removes every Match presentation in its first frame without changing the persisted operator mode.

The reproducible pre-existing Media multi-label relative-p95 failure is a hard predecessor, not a Match exception. Match release remains blocked until an independent reviewer either proves the canonical gate resolved or attributes the failure as pre-existing from a same-binary Match-unavailable baseline with raw comparable evidence. Match work may not hide, waive, suppress, loosen, or relabel that gate.

</topic>

<topic id="research-basis" status="active" version="2" wp="WP-087" summary="Facial's existing build rules require shared rendering, structured routes, exact visual inspection, explicit sensitive-capture authorization, full failure paths, independent adversarial review, and packaged-runtime proof." updated_at="2026-10-08">

## Authority and reuse

- 2026-10-08 native Media predecessor tooling: inspected pinned eframe 0.27.2 `epi.rs` IntegrationInfo::cpu_usage and `native/glow_integration.rs` paint/tessellation timing; native CPU includes update and backend rendering, excludes vsync. Reuse `match_benchmark.rs` bounded writer and `ui.rs` exact 50,000-key empty/five-label helpers in four isolated background native sessions, baseline/candidate/candidate/baseline. Reject headless Context::run as packaged-native proof. Require a fresh empty workspace, actual 1920x1080/100-percent/ppp1/Inter19 observations, process-lifetime Match admission counts, immutable fixture/source/executable hashes, and unchanged historical inspector/10-percent p50+p95/16.7-ms p95 gates. Risks: persisted window geometry, operator-data mutation, stale declared admission or display, sample loss, controller focus, and fixture drift; mitigate with isolated config, persistence/controller suppression, live cumulative assertions, bounded fail-closed output and common fixture hashing. Validate focused config/path/display/counter boundaries, raw native ABBA reconciliation, exact framebuffer, existing canonical inspector gate, and final guarded full suite/package proof; no calibration or recall exception.

- 2026-09-30 interval evidence: verified primary [Instant](https://doc.rust-lang.org/std/time/struct.Instant.html), [OnceLock](https://doc.rust-lang.org/std/sync/struct.OnceLock.html), and [VLC 3 media-player header](https://github.com/videolan/vlc-3.0/blob/master/include/vlc/libvlc_media_player.h). Reuse one process-local monotonic epoch, existing governor accounting lock/RAII releases, and the existing native player poll before optimistic reconciliation. Rotate fixed-size numeric interval peaks/counters only on dedicated diagnostics; seed peaks with opening leases. Keep bounded numeric activity rings and reject sequence loss/overflow. Label admitted leases separately from kernel execution and raw native playing/clock observations separately from frame presentation. Reject lifetime peaks as exact interval proof, UI playing targets as native observations, and invented visible-work thresholds. Validate interval-spanning leases, transient between-poll peaks/pressure, contiguous rotations, tagged lease release, shared clocks, missing native clocks, and bounded record loss. These diagnostics do not alter admission, playback holds, safe-unit deadlines, or workload acceptance.

- 2026-09-30 diagnostics finalization: verify primary [SyncSender::try_send](https://doc.rust-lang.org/std/sync/mpsc/struct.SyncSender.html#method.try_send) and inspect `api::mark_intent_applied` recovery semantics. Use one lazy receipt writer with a 32-item `sync_channel`; UI uses nonblocking `try_send`, skips lock-taking state capture, and preserves processing claims on queue/I/O failure. Clone the ready GUI-owned store under the service mutex, then release that mutex before database reads. Reject blocking sends and per-receipt threads, which can stall the UI or accumulate without a bound. Validate an actually held service mutex across the diagnostic deadline and queue pressure.

- 2026-09-30 canonical stage diagnostics: inspect `JobAsset.next_stage`, `JobStage::ORDERED`, and existing grouped failure-count queries; verify [SurrealQL grouped SELECT](https://surrealdb.com/docs/reference/query-language/statements/select) and [count](https://surrealdb.com/docs/reference/query-language/functions/database-functions/count). Reuse a background aggregate returning at most seven known stage counts. Expose the scope as persisted next-asset stages, including paused/history rows, rather than claiming a currently executing native operation. Reject unknown stages or malformed counts; never materialize media keys or source paths. Validate empty, multiple-stage, restart, and privacy cases through the existing public diagnostic test target. No schema migration or new index is needed; a large aggregation that misses the diagnostic deadline remains an unavailable sample.

- `CODEX.md` sections 6, 7.1, 7.2, 8, and 8.3.
- `governance/build_rules.yaml` model operation, GUI, diagnostics, persistence, review, and packaging gates.
- Existing `ui-inspect`, `ui_snapshot`, receipts, Manual, release script, and executable-layout validator.
- Existing Media frame diagnostics, Viewer metadata-band contract, chrome-hidden state, native-video placement/capture diagnostics, virtualized viewports, and background-safe intent routes.

Selected approach: assemble a complete state/failure matrix, fill missing intents/diagnostics/fixtures, run exact large-library and recovery drills, independently review high-risk producer-consumer boundaries, then package and re-prove the same contract from extracted artifacts.

## Non-render benchmark collection research (2026-09-30)

- Sources checked: the pinned eframe 0.27.2 `IntegrationInfo` API and local pinned source define `cpu_usage` as previous-frame `App::update` plus backend rendering, excluding vsync; Rust `Instant` documents a monotonic nondecreasing clock for benchmark durations; Python `time.monotonic_ns()` provides integer nanosecond timing; Python `subprocess.run` supports argument-vector invocation, explicit timeouts, captured output, and return-code checks. Sources: <https://docs.rs/eframe/0.27.2/eframe/struct.IntegrationInfo.html>, <https://doc.rust-lang.org/std/time/struct.Instant.html>, <https://docs.python.org/3/library/time.html#time.monotonic_ns>, <https://docs.python.org/3/library/subprocess.html#subprocess.run>.
- Relevant project patterns: UI operations use file-based commands and terminal receipts; Match route actions apply on GUI frames, and `match_status` supplies state/resources but initializes Match storage. The current source now exposes receipt-backed `match_settings_manage_people`, `match_editor_autocomplete`, and read-only same-GUI `match_runtime_diagnostics`; autocomplete validates the exact active media/editor/catalog revision and does not substitute `MediaSearch`. The diagnostics route exposes the running GUI's `public_snapshot` without initializing a second Match store.
- Interaction seam research: inspect the existing Settings button handler, autocomplete worker-result application/receipt path, and pinned egui 0.27.2 `Response::changed` source (`egui/src/response.rs`), which distinguishes actual input changes from view-only changes. Reuse the exact route handlers and shared post-`Context::run(render_ui)` finalizer so receipts bind to rendered UI state without terminal I/O inside paint; report only the existing explicit scope `ui_state_rendered_by_render_ui_excluding_backend_and_vsync`.
- Reuse: invoke the configured `facial-cli` as an argument vector, measure caller-observed command-to-terminal-receipt completion with Python's monotonic clock, and preserve exact endpoint/action and receipt outcome in bounded JSONL. Concurrent sampling uses the same-GUI diagnostics route plus coherent governor lifetime telemetry; absent indexing-stage and thumbnail/playback/navigation latency producers remain explicit nulls, and this collector cannot prove disabled admission.
- Rejected: deriving autocomplete latency from `MediaSearch`; treating CLI admission/accepted receipts or RFC3339 receipt timestamps as endpoint completion/monotonic duration; interpreting absent status counters as zero; and claiming `match_status` polling proves a no-admission baseline.
- Selected approach: bounded standalone collector/analyzer with a closed protocol schema, explicit endpoint-to-command mapping, 20 warmup plus 200 measured calls per available endpoint, a 60-second warmup plus 600-second concurrency capture, raw-record hashes, and fail-closed missing/unsupported telemetry. Keep intent-to-terminal latency separate from render samples.
- Risks and mitigations: a CLI subprocess or status poll perturbs the measured system, so record collector identity/cadence and classify the metric as end-to-end intent latency; unsupported exact endpoints remain blocked until a real route is implemented; any missing latency/ceiling/lease field prevents a passing saturation verdict rather than being inferred.
- Validation: Python tests cover exact schema, bounds, nearest-rank summaries, endpoint/receipt mismatch, missing exact routes and telemetry, duplicate/unknown JSON fields, out-of-order timestamps, incomplete run/end records, and hash mismatch. Actual performance and resource results still require the packaged runtime, fixtures, hardware, and WP-082/WP-086 predecessors.

## Governor lifetime telemetry research (2026-09-30)

### Existing visible-work endpoint producers (2026-09-30)

- Sources inspected: current `ui.rs` Visible thumbnail requests, `paint_media_tile` texture drawing, `navigate_grid` cursor changes, and raw native `video_player.rs` snapshots; [Rust Instant](https://doc.rust-lang.org/std/time/struct.Instant.html) and [VideoLAN VLC 3 media-player header](https://github.com/videolan/vlc-3.0/blob/master/include/vlc/libvlc_media_player.h) distinguish monotonic durations and native playback time from physical frame presentation.
- Reuse the exact existing endpoints: first Visible-priority thumbnail request to its matching texture paint; grid navigation request to its matching cursor tile paint; accepted explicit native seek to a displaced raw LibVLC clock observation. Reject optimistic snapshots and ordinary forward clock progress as seek evidence. GUI endpoints exclude backend/vsync; native-clock endpoints exclude decoded-frame/physical-presentation claims.
- Keep 256 pending observations and 256 numeric-only samples per series, with UUID lifetime, sequence, monotonic start/end/duration, pending/abandoned counts, history-drop count, and fail-closed clock/overflow state. Scope thumbnail/navigation by exact tab, scan, and cache key/target; never serialize those keys, media paths, names, geometry, crops, or seek targets. Record only memory during paint and attach cached numeric samples to the existing read-only same-GUI diagnostics receipt outside paint.
- Rejected: command return time as seek completion, texture upload alone as visible paint, warm-cache zeros, repeated polling of a last-value latency as independent samples, and invented budget thresholds. Consumers must reject lifetime/reset/sequence gaps and use only newly observed samples; real 600-second concurrency, saturation, and existing visible-work budget acceptance remain independent required proof.
- Validation: focused bounded-series tests cover exact endpoint identity, first-start preservation, duplicate completion rejection, privacy redaction, sequence loss, clock reversal, capacity overflow, and optimistic/naturally advancing/error seek observations. Runtime hook tests bind actual visible texture/cursor paint and raw pre-reconciliation player observations to these producers.

- Sources checked: current `product/src/match_store.rs` implements `MatchResourceGovernor` with an `Arc<Mutex<ResourceUsage>>`; acquire, atomic replace, release, current usage, and poisoning checks already share that accounting lock. Rust `Mutex` serializes ownership and reports poisoning; Tokio `RuntimeMetrics` documents the distinction between live gauges (such as queue depth) and monotonically increasing lifetime counters. Sources: <https://doc.rust-lang.org/std/sync/struct.Mutex.html>, <https://docs.rs/tokio/latest/tokio/runtime/struct.RuntimeMetrics.html>.
- Selected approach: keep fixed-size native governor lifetime telemetry beside existing usage under the same lock: coherent current usage, per-axis high-water values, successful acquire/replace/release counts, pressure count, overflow state, and stable lifetime identity. Do not add a metrics dependency or treat sampled current usage as peak evidence.
- Risks and controls: lifetime peaks include any work before the exact benchmark interval, so label them as conservative lifetime peaks and retain independent interval-visible-work proof; compare baseline/end lifetime identity and monotonic counters, and reject identity changes, resets, overflow, poisoned telemetry, or incomplete deltas. The lifetime high-water record supplements rather than replaces exact indexing, playback, thumbnail, and navigation budget evidence.
- Validation: cover a transient lease entirely between external polls, pressure/failure paths with unchanged lease accounting, successful replacement/preparation and RAII release, cloned governor identity, new-governor identity, counter overflow, and poisoned/invalid telemetry. Saturation acceptance still requires exact configured ceilings, observed peaks, backpressure, terminal zero lease balance, and each existing visible-work verdict.

### WP-087 bounded render-sample collector research

- Sources checked: `product/Cargo.lock` pins `eframe` 0.27.2; the corresponding local `eframe-0.27.2/src/epi.rs` documents `IntegrationInfo.cpu_usage` as the previous frame's seconds including `App::update` and backend rendering except vsync; [Rust `Instant` documentation](https://doc.rust-lang.org/std/time/struct.Instant.html) defines a monotonically nondecreasing clock suitable for elapsed-time measurement. The versioned [docs.rs eframe IntegrationInfo page](https://docs.rs/eframe/0.27.2/eframe/struct.IntegrationInfo.html) was requested but was inaccessible through the web reader; pinned local crate source was inspected directly.
- Pattern selected: collect only in an explicit 150-second run, keep the 30-second warmup outside the 120-second sample interval, pass prior-frame duration and current monotonic observation time from the UI, then send a small sample through a bounded nonblocking channel to a dedicated writer. File creation and all serialization/disk I/O stay off the paint path; a full queue invalidates the run instead of dropping records.
- Rejected: reusing existing rolling summary diagnostics because they lack timestamped raw frames and cannot reconstruct every required overlapping window; synchronous paint-path writes because they add I/O and serialization to the measured path; claiming eframe CPU duration represents vsync wait or physical display presentation.
- Risks and controls: enforce a safe run ID, 64 KiB config cap, create-new file semantics, canonical workspace containment and reparse checks; cap capture at 100,000 records and 20 MiB; treat unknown admission counts or absent independent disabled-state evidence as ineligible; mark queue overflow, worker failure, or incomplete duration as failed evidence.
- Validation plan: unit-test config and run-ID rejection, path/reparse/containment checks, bounded queue overflow, warmup/measurement time boundaries, sample encoding and writer failure; separately validate the analyzer's half-open rolling windows, nearest-rank percentiles, spike handling and comparison invariants. Runtime release proof still requires a packaged run and independent canonical review.

Presentation proof uses generated media and fictitious People in deterministic fixtures. Exact-live state is inspected only through the existing background-safe route. Exact pixels are never silently redacted: when a Match framebuffer contains sensitive presentation, ordinary `ui_snapshot` rejects with `sensitive_capture_authorization_required`; explicit `--include-sensitive-match` captures the exact unchanged framebuffer, constrains the request to the named surface, and marks the receipt and output privacy-sensitive. Frame proof compares Match disabled, active-but-quiet, and explicit editing rather than accepting a fast empty fixture. Match disabled means unavailable or unconfigured with zero Match worker, model-load, or index-query admission; operator-paused is measured separately and cannot stand in for disabled.

</topic>

<topic id="red-team" status="active" version="2" wp="WP-087" summary="Happy-path polish, sensitive diagnostics, falsely exact redacted snapshots, incomplete resets, async misattribution, and source-only proof can create a false release." updated_at="2026-08-23">

## Risks and controls

- Happy path hides failure corruption: inject model, DB, cancellation, restart, import, undo, reset, and package failures.
- Face data leaks into diagnostics: redaction guards and bounded explicit opt-in payloads.
- `rebuild_match_analysis` deletes operator truth or `clear_all_match_data` cannot restore it: retain WP-085's typed modes, exact affected-state manifests, verified recovery bundle, state-bound confirmation, exact durable-graph restore, independent excluded-vector/crop rebuild, and restart proof.
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
- Visual evidence leaks operator identities or redaction invalidates exact proof: synthetic fixtures, default sensitive-frame refusal, seeded canary scans, and explicit bounded privacy-marked exact-live capture without pixel alteration.
- Native video covers or desynchronizes a face editor: accept metadata track/timestamp editing or a paused egui-owned captured still only after the native child withdraws.

</topic>

<topic id="presentation-proof-matrix" status="active" version="2" wp="WP-087" summary="Release proof covers quiet browsing, opt-in correction, dense faces, large People, fullscreen, focus safety, native video, frame time, and privacy without operator-data leakage or falsely exact redacted pixels." updated_at="2026-08-23">

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
- Non-visual diagnostic fields redact sensitive Match values by default. Exact-live pixels are never modified and still called exact.
- Ordinary `ui_snapshot` rejects a framebuffer containing sensitive Match presentation with structured `sensitive_capture_authorization_required` and writes no image.
- Explicit `ui_snapshot --include-sensitive-match` is limited to the named requested surface, returns the exact unchanged framebuffer, marks the receipt/output privacy-sensitive, and is never used as an ordinary background artifact.

</topic>

<topic id="microtask-plan" status="active" version="2" wp="WP-087" summary="Complete the state matrix, fill structured tooling gaps, prove typed recovery and privacy-exact visuals, then package and independently audit." updated_at="2026-08-23">

## Microtasks

1. Build the full feature/state/failure/acceptance matrix from WP-080 through WP-086.
2. Add the quiet, committed-assignment-row, single/dense-face, correction-vocabulary, compact/high-font, virtualized Match -> People 10,000, Settings-route-with-zero-People-rows, fullscreen-editor-closed, video-correction, failure, and privacy fixtures through the shared render path.
3. Add missing pause/focus/privacy intents, default sensitive-frame refusal, explicit `--include-sensitive-match`, diagnostics, snapshots, state fields, and Manual instructions.
4. Run focused/full tests, correction-semantic and pause/fullscreen interleavings, the v1 A/B benchmark, prohibited-paint-work and resource-governor saturation/lease probes, the Media multi-label predecessor audit, scale/starvation probes, privacy canary scans, both WP-085 reset modes, and recovery drills.
5. Inspect all deterministic states directly and run only bounded privacy-safe exact-live proofs without foreground activation.
6. Run independent adversarial review and resolve every high-risk finding.
7. Package, extract, re-prove Match including the presentation/privacy matrix, then synchronize spec/topology/taskboard/packet status.

</topic>
