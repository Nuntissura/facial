---
file_id: REF-WP-086-MATCH-VIDEO-CONTEXT-ACCELERATION-V1
file_kind: refinement
updated_at: "2026-09-06"
---

<topic id="operator-request" status="active" version="1" wp="WP-086" summary="Extend the proven image Match workflow to video appearances, review-only context, and production-scale acceleration without weakening identity truth." updated_at="2026-08-22">

## Scope contract

Video appearances belong in person galleries with seekable timestamps, but frames must not flood the index. Context can prioritize reviews but cannot confirm identity. Acceleration is accepted only with CPU parity, reproducible packaging, fallback, and quiet background behavior.

Video playback and immersive fullscreen are transient Match pause reasons, not persisted operator-mode changes. Normal playback remains visually clean and never prompts, boxes faces, steals focus, or seeks for review. Video correction is explicit and bound to a stable track ID plus exact timestamp.

Hold admission is bounded separately from in-flight completion: after a hold is requested, no new Match stage may start after 250 ms, and an already admitted checkpointable safe unit has a hard 2,000 ms wall-clock bound from execution start to checkpoint or terminal outcome. Non-cooperative work must be split or run behind a supervised app-owned, no-window Rust-native isolated worker boundary; it may never occupy UI, playback, visible-thumbnail, render, or foreground database-writer resources indefinitely.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-086" summary="Scene sampling, within-shot tracks, pose-diverse exemplars, and multi-frame aggregation avoid poster-frame misses and per-frame index explosion." updated_at="2026-08-22">

## Sources and selected approach

- FFmpeg scene filters: https://ffmpeg.org/ffmpeg-filters.html
- Neural Aggregation Network: https://openaccess.thecvf.com/content_cvpr_2017/html/Yang_Neural_Aggregation_Network_CVPR_2017_paper.html
- ONNX Runtime DirectML reference for comparison only: https://onnxruntime.ai/docs/execution-providers/DirectML-ExecutionProvider.html
- Rust tract runtime: https://github.com/sonos/tract
- `CODEX.md` sections 7.2 and 8.1 plus `governance/build_rules.yaml` playback/model-operation rules.
- `product/src/video_player.rs` and `product/src/ui.rs`: the decoded frame is a clipped native child with one reconciled owner; its native z-order means an ordinary egui overlay over active playback is not an established presentation path.

Combine scene changes with bounded time sampling, track within a shot, retain high-quality pose-diverse exemplars, and weight one track once for clustering. Store visual and context evidence separately. Benchmark the latest tract GPU/CPU path first; any alternative native runtime requires an explicit architecture and packaging decision.


### Implementation research update — 2026-09-06

- Inspected pinned `tract-0.23.5/src/lib.rs`, `identity.rs`, service job admission, `video_player.rs`, thumbnail extraction, Match corrections, exchange and recovery. Synchronous preparation, startup inference and decode/inference lack cooperative cancellation; all must execute under the supervised safe-unit deadline, including preparation.
- Rust Child lifecycle: https://doc.rust-lang.org/std/process/struct.Child.html ; Windows process attributes and Job Objects: https://learn.microsoft.com/en-us/windows/win32/api/processthreadsapi/nf-processthreadsapi-updateprocthreadattribute and https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects . Selected: explicit packaged Rust worker entrypoint before configuration/service startup, bounded framing, owned process/job containment, no window, monotonic deadline, generation fencing, quarantine and fresh-worker retry. Queue wait cannot disguise executing preparation. Failed or slow preparation must remain a visible failure until a compliant measured implementation is available.
- LibVLC 3 pixel callbacks do not carry frame PTS: https://raw.githubusercontent.com/videolan/vlc/3.0.x/include/vlc/libvlc_media_player.h . The active Viewer snapshot/get_time path cannot prove exact frame correspondence and is rejected for background sampling. Existing hidden FFmpeg extraction is a reuse candidate, but a decoder must return actual PTS/timebase and remain supervised; requested seek time is not evidence. New native decoder libraries require an explicit packaging decision.
- Tracking patterns: https://raw.githubusercontent.com/Breakthrough/PySceneDetect/main/scenedetect/detectors/content_detector.py and https://raw.githubusercontent.com/FoundationVision/ByteTrack/main/yolox/tracker/byte_tracker.py . Use bounded low-resolution temporal/scene sampling, conservative within-shot association, deterministic pose-diverse exemplar selection, explicit ambiguity termination, and resumable checkpoints. Existing duplicate-family clustering can count one track once; it must not count every frame.
- Current image Face IDs must remain unchanged. Video provenance requires actual stream/PTS/timebase and stable track membership rather than repeated frame-local source indexes. Reuse transactional correction deltas and undo, with complete track membership/revision/timestamp fences, exact counts, the existing 4096-row atomic limit, and explicit split without changing per-face truth.
- Context stays separate from visual similarity and automatic gates. Store folder, filename, time, album and co-occurrence sources independently; ablation must leave assignments, cannot-links and trust unchanged. Existing NAN aggregation research supports quality selection, not new identity thresholds.
- Reject unbounded pipes, shared Viewer/thumbnail decoder ownership, in-process non-cooperative work, asynchronous result publication without full revision fences, and CUDA-enabled-as-proof claims. Validate hung preparation/decode/inference, backpressure, worker/parent crash, late results, fresh retry, exact CFR/VFR frame markers, restart/split/undo, context ablation and actual CPU/accelerated parity/throughput before completion.

### Paused loop restoration research - 2026-09-08

- Exact VLC 3.0.23 `src/input/input.c` lines 586-590 and 867-891 reuse `start-time` as the repeat origin; it cannot restore an arbitrary position without changing loop semantics. Lines 640-657 support `start-paused`, but the bounded mixed-origin fixture probe remained paused with no vout for the existing ten-second deadline.
- Source: https://raw.githubusercontent.com/videolan/vlc/3.0.23/src/input/input.c. Actual Rust verbose evidence passed both first-player captures and failed only after immediate paused loop reconstruction; snapshot source reports absence of a picture, not a path or conversion error.
- Preserve the existing paused player and frame while recording the desired loop option. Rebuild only on explicit Play using the latest position and existing source pin/profile; advance restoration through polling with ten-second readiness and three-second seek bounds. Never rebuild from polling. New operator requests supersede pending restoration; failure remains visible and unconfirmed until a successful rebuild/reload or Stop clears it.
- Validate toggle cancellation, pending Pause, latest post-toggle seek, single explicit-Play rebuild, source/profile retention, preference restoration and native mixed-origin captures without increasing existing gates. No native frame-exact transport claim replaces the separate exact-still proof.
- Resume research (2026-09-30): VLC 3.0.23 `src/input/input.c` 1892-1937 resets decoder state during seek; paused demux can lack a picture despite vout/native clock confirmation. `lib/media_player.c` 1785-1792 and `src/input/input.c` 2191-2205 provide an explicit paused frame request. Candidate: confirm Pause before restored seek and request one frame per dispatched paused seek, retaining existing deadline and clock tolerance; require the replacement blue capture before acceptance. Pause-before-seek alone failed the native replacement capture.

### Bounded CPU executor candidate research — 2026-09-08

- Sources: published `tract-linalg-0.23.5/src/multithread.rs` (Executor, private two-thread pool, scoped TLS override), `tract-core-0.23.5/src/runtime.rs` and `plan.rs` (default executor fallback), and `tract-linalg-0.23.5/src/frame/mmm/mod.rs` (private-pool tile dispatch); upstream https://github.com/sonos/tract . Current docs.rs retrieval was unavailable; exact pinned package source was inspected locally.
- Reuse: unchanged CPU graph, verified model bytes/generation, checkpoint preparation, isolated worker, existing parity tolerances and Job memory containment. The single-thread default remains selected. CUDA diagnostics remain separate and unpromoted while total GPU memory containment is unproven.
- Selected diagnostic: explicit `--cpu-two-thread` runs a separate private two-thread CPU executor, initialized in one supervised two-second operation after reserving two CPU units and resident memory before launch. Scope every preparation/inference call; never use Rayon global or change the process default. Worker exit owns executor/resource cleanup; a panic terminates the worker instead of reusing TLS state.
- Risks/controls: thread-pool memory is covered by the existing process Job cap; admission reserves both CPU units, a timed-out child retains leases until confirmed dead, runtime evidence identifies this policy separately, and neither speed nor parity alone promotes it. Enabling the pinned optional kernel feature must retain baseline parity too.
- Scope limit: two CPU units remain held for this isolated diagnostic worker lifetime. Production selection would need per-active-unit admission and idle-pool accounting; this diagnostic does not change production scheduling or defaults.
- Resume integration (2026-09-30): reuse the same pinned private executor inside the production worker as an explicit unpromoted policy. Reserve two CPU units per active initialization/preparation/inference unit; retain resident memory with the idle worker and release compute admission after each bounded operation. Bind the pool to model generation, retain full request fences, and confirm prior-worker exit before admitting replacement memory on policy or generation change. Prove mixed image/video jobs, later-asset reuse, changed-generation replacement, holds, idle accounting, real-model parity, throughput and packaged fallback before selecting the policy by default. CUDA remains unpromoted because its effective memory cap is not proven.
- Proof: actual same-input baseline/candidate preparation and repeated inference under unchanged deadlines, numerical/geometry/failure parity, cold startup and throughput timings, measured process/Job peaks, confirmed exit and CPU token release. Focused tests reject conflicting CLI options and mislabeled candidate evidence; native owned-worker test verifies exactly two private pool threads and baseline restoration. No acceptance thresholds change.

### Shared database-owner boundary research — 2026-10-01

- Sources checked: pinned local `surrealdb-3.2.4/src/method/query.rs` (`Query::bind`, `IndexedResults::take`/`check`/`take_errors`), `surrealdb-types-3.2.4/src/value/mod.rs` (the tagged `Value` derives Serde), `surrealdb-core-3.2.4/src/kvs/surrealkv/mod.rs` (background flush, commit coordinator, WAL flush and async close), [SurrealDB Rust transactions](https://surrealdb.com/docs/reference/rust/concepts/transaction), [SurrealDB transaction semantics](https://surrealdb.com/docs/learn/querying/concepts-and-guides/transactions), [Windows Job Objects](https://learn.microsoft.com/en-us/windows/win32/procthread/job-objects), [Windows named-pipe overlapped I/O](https://learn.microsoft.com/en-us/windows/win32/ipc/named-pipe-open-modes), and [Rust Child lifecycle](https://doc.rust-lang.org/std/process/struct.Child.html). Project evidence: `product/src/surreal_store.rs`, `surreal_kv.rs`, `media_db.rs`, `match_store.rs`, `match_store/worker.rs`, `match_worker.rs`, `match_decoder_process.rs`, and `lib.rs`.
- Observed gap: Media and Match open the same embedded SurrealKV root through one process-local `surreal_store::Store` and `RwLock`. Match `persist_write_guard` bounds only lock acquisition and explicitly leaves recovery, queries, commit, and failure recording unbounded. Dropping a query future does not prove embedded execution stopped. The existing hidden Rust worker and Windows Job/pipe containment provide a process pattern, but the compute worker never opens the database. A second embedded process must never open the live root concurrently.
- Codec decision from pinned source inspection: `serde_json-1.0.150/src/ser.rs:2079–2132` scans a whole unescaped string before the next writer call, so a deadline-aware JSON writer cannot bound a permitted 64 MiB Match binding. The already locked `ciborium-0.2.2/src/ser/mod.rs:158–167` streams string/byte headers and bytes directly to its writer ([Ciborium `into_writer`](https://docs.rs/ciborium/0.2.2/ciborium/ser/fn.into_writer.html)); `rmp-serde-1.3.1/src/encode.rs:648–650` also delegates direct string writes, but its default positional struct encoding adds a schema-evolution hazard ([RMP struct-map documentation](https://docs.rs/rmp-serde/1.3.1/rmp_serde/config/struct.StructMapConfig.html)). Select pinned Ciborium CBOR for private wire version 2, with typed Surreal `Value` preservation and a 128 MiB frame cap. Parent digest and request serialization use an original-deadline writer that checks and copies/hashes at most 64 KiB per step; timeout before dispatch has no commit uncertainty and cannot retire a healthy owner. Reject detached, uncharged encoder threads. Proof requires production-owner typed round trips (`NONE`, null, RecordId, Decimal, bytes, nested values), deliberately paced large-string encoding within the original 2,000 ms return deadline, no late dispatch/owner retirement, and malformed/trailing-frame rejection. The source-level choice does not itself prove arbitrary Match producer work before query construction meets the same deadline.
- Selected implementation: keep one actual embedded engine in a supervised hidden Rust-native database-owner child, entered before GUI/config startup. Implement protocol, parent client/supervisor, and child runtime as separate `product/src/database_owner/**` modules; use a thin `surreal_store` routing facade and narrow Media KV/Match adapters so app code, behavior, and feature ownership remain modular. No whole-workspace crate restructure is implied. The facade preserves `query(...).bind(...).await` and indexed response `take`/`check`/`take_errors` semantics, plus the current schema/session and transaction-lock requirements; migrate other live-root callers, including timeline/inspector paths where applicable, before claiming single ownership. Parent/client owns admission epoch, priority lane, queue deadline, operation ID, workspace/root identity and bounded frame sizes. The owner receives typed `surrealdb::types::Value` bindings and returns ordered typed statement values plus indexed errors through versioned, length-prefixed Serde frames; conversion uses `SurrealValue::into_value` and `SurrealValue::from_value`, preserving `NONE`, record IDs, decimal/float, bytes and geometry rather than flattening through `serde_json::Value`. Probe exact pinned 3.2.4 round trips before converting call sites.
- Priority and deadline: separate Media foreground and Match background queues with bounded capacity; owner admits foreground before the next Match unit, never holds a parent-side foreground writer permit while Match waits, and rechecks hold/epoch immediately before a Match query. One admitted Match database unit carries its original monotonic 2,000 ms execution deadline through recovery, query, commit, response, and failure recording; queue time is reported separately. A timed-out owner is fenced immediately and stopped only through its app-owned Job. Confirm exit and released database lock before one fresh owner opens the canonical root. Do not call an unfinished commit cancelled: distinguish precommit abort, acknowledged commit, and unknown commit, then reconcile an unknown outcome by operation ID/revision from a fresh canonical read before retry or publishing. Commit the operation receipt with its domain mutation where atomicity is required; use a parent-owned bounded failure journal for timeout/crash evidence until the fresh owner can record it, and surface a retryable failure if recording still cannot complete. No late response from a retired owner/epoch may alter parent state.
- Rejected approaches: `tokio::time::timeout` or dropped futures over the current embedded handle; per-call threads; killing the GUI process; a Match-only database process with Media still holding the embedded root; a remote Surreal server or separately packaged executable. Each misses forceable full-engine isolation, single-root ownership, packaging simplicity, or the existing query surface.
- Risks and controls: a hung Match query can block the single owner and briefly make Media unavailable, so foreground admission/preemption and owner restart latency must be measured against actual Media budgets; if those budgets fail, this design is not proven. Parent/owner crash and pipe EOF must fence all in-flight requests, bound memory, and reopen/reconcile before accepting new mutations. A commit acknowledgement lost across process death must never be retried blindly. Schema migration, HNSW rebuild, and large write batches must be split or treated as visible failures when they cannot meet the safe-unit deadline; response framing must cap individual and aggregate bytes and reject malformed protocol/version/root identities. Keep local-root portability and no-window startup.
- Validation: pinned-type codec/statement-error parity; single-owner and no-second-engine probes; exact Media/Match canonical row parity; fault injection at recovery, query, before commit, after commit before acknowledgement, failure recording, pipe break and owner crash; 2,000 ms terminal outcome including owner startup/reopen within an admitted unit; lazy Match initialization uses background SQL requests with a per-request 2,000 ms deadline but has no enclosing filesystem/open deadline; 250 ms hold admission; late-result rejection and confirmed Job exit; priority and responsive playback/thumbnail measurements under a hung Match query; fresh owner retry only after canonical reconciliation; packaged portable/setup hidden-child and workspace relocation proof. Focused checks precede the required final WP suite and package proof.

Selected presentation: keep committed manual or strict-automatic People assignments with provenance and explicit track/timestamp actions in the Viewer metadata band. An optional box editor may operate only after playback is paused, the native child withdraws its surface claim, and an exact timestamp still is rendered in an egui-owned correction surface. Otherwise correction remains metadata-only. This preserves the current single-owner native-video contract and makes visual correspondence testable.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-086" summary="Frame floods, wrong contextual reinforcement, timestamp drift, UI starvation, and native-runtime packaging failures are the main risks." updated_at="2026-08-22">

## Risks and controls

- Frame flood: bounded exemplars and one-track density weight.
- Scraped-name reinforcement: context remains review-only and removable.
- Wrong seek: exact time-base fixtures and Viewer proof.
- Indexer starves playback: WorkClass::Background, bounded queues, cancellation, playback probes.
- Acceleration mismatch: frozen output tolerance, CPU fallback, packaged-runtime test.
- Native child occludes or desynchronizes an egui editor: never overlay active LibVLC; edit through track/timestamp rows or a proven captured-still surface after withdrawing the child.
- Pause-reason race resumes Match unexpectedly: persist operator mode separately, represent playback/fullscreen as attributable holds, and generation-fence admission/resume.
- Hung synchronous stage survives a hold: cap safe units at 2,000 ms, isolate non-cooperative calls, quarantine timed-out workers/generations, reject late results, and retry only in a fresh worker.
- Tracker crosses two people: preview track scope and counts, retain source observations, support split, and exclude unreviewed track edits from trusted exemplars.

</topic>

<topic id="presentation-and-pause-contract" status="active" version="1" wp="WP-086" summary="Video Match stays quiet during playback, pauses for playback/fullscreen through attributable holds, and edits exact tracks/timestamps without an unproven overlay over native LibVLC." updated_at="2026-08-22">

## Effective pause reasons

- `viewer_playback` is active while the existing playback lease is preparing, buffering, playing, or seeking. It admits no new Match stage and asks a bounded in-flight stage to checkpoint safely.
- `immersive_fullscreen` is active from the first hidden-chrome frame until immersive mode ends. It removes every Match row, box, editor, prompt, and badge and admits no new stage.
- Holds are a reason set layered over the persisted operator mode. Releasing one reason cannot clear another reason or change an operator pause. Resume requested under a hold becomes eligible only after the last hold clears and only if the operator mode permits it.
- Receipts and diagnostics expose persisted mode, active reasons, effective state, checkpoint state, and generation without exposing names, crops, regions, or vectors by default.

## Bounded safe units and isolation

- Hold requests stop new stage admission within 250 ms. Queue wait is reported separately and cannot be mislabeled as an executing safe unit.
- A safe unit is a checkpointable Match work item that reaches a checkpoint or terminal outcome no later than 2,000 ms after execution starts. Decode batches, frame samples, inference batches, and persistence batches must be split to honor that limit.
- Match safe units use dedicated bounded Background resources and never hold UI, playback, visible-thumbnail, render, or foreground database-writer permits.
- If a unit reaches 2,000 ms, the coordinator records retryable `safe_unit_timeout`, fences the operation and generation, rejects every late result, quarantines the worker and affected model/runtime generation, and releases its owned leases.
- Any decoder/runtime call that cannot guarantee cooperative return within the bound runs in a supervised app-owned, no-window Rust-native isolated worker. The supervisor may stop only the child it created; retry uses a fresh worker after explicit or bounded policy-driven retry, never the quarantined worker.
- Fullscreen exit removes only `immersive_fullscreen` and returns to the ordinary Viewer with **Edit faces** closed. It never revives discarded drafts, boxes, selections, or autocomplete state.

## Video correction presentation

- Ordinary playback never displays a face box, naming field, or suggestion and never changes playback position for Match.
- The metadata band can list committed manual or strict-automatic People assignments with provenance and stable appearance timestamps and can seek only after an explicit operator action.
- A correction selects a stable track ID and exact timestamp before changing transport state. It either remains a metadata timestamp/track editor or pauses playback, withdraws the native child, and presents the exact captured still in an egui-owned correction surface.
- Face geometry and hit targets are accepted only when the displayed still, timestamp, asset, track generation, and normalized regions reconcile. A stale frame or generation rejects the edit.
- Track-wide actions preview affected observations, exemplars, and timestamps. A mixed track must be splittable; no correction silently expands to other tracks or media.

</topic>

<topic id="microtask-plan" status="active" version="1" wp="WP-086" summary="Prove tracks on CPU before context and acceleration." updated_at="2026-08-22">

## Microtasks

1. Define video sampling, track, exemplar, timestamp, and density contracts.
2. Implement bounded CPU pipeline and deterministic fixtures.
3. Add reason-aware playback/fullscreen holds, 250 ms admission cutoff, 2,000 ms safe-unit deadlines, isolated-worker quarantine/retry, effective-state diagnostics, and interleaving tests.
4. Add seekable person-gallery appearances and diagnostics.
5. Add metadata timestamp/track correction plus the captured-still editor only if native-child withdrawal and frame/geometry correspondence prove it safe.
6. Add track-scope previews, contaminated-track split, and stale-generation rejection.
7. Add separately stored context evidence and ablation proof.
8. Benchmark/select acceleration and prove parity/fallback/packaging.
9. Run combined indexing/playback/fullscreen/background-safety scale tests.

</topic>
