---
file_id: REF-WP-086-MATCH-VIDEO-CONTEXT-ACCELERATION-V1
file_kind: refinement
updated_at: "2026-08-22"
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
