---
file_id: REF-WP-057-REMOTE-MEDIA-IO-PLAYBACK-PRIORITY-V1
file_kind: refinement
updated_at: "2026-08-09"
---

<topic id="operator-request" status="active" version="1" wp="WP-057" summary="Keep visible media and embedded playback responsive while background NAS work continues." updated_at="2026-08-09">

## Operator request

- Implement the approved shared remote-I/O budget across scans, metadata, thumbnails,
  and video-related background work.
- Prioritize visible thumbnails and active embedded playback over prefetch and bulk work.
- Cache proven mapped-drive/UNC root resolution instead of resolving it per thumbnail.
- Improve diagnostics sufficiently to tune behavior from evidence.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-057" summary="SMB queueing, mature media-library worker controls, and LibVLC options support bounded adaptive arbitration." updated_at="2026-08-09">

## Sources and patterns checked

- Microsoft documents that large storage queues increase measured latency and that
  concurrent shared-storage workloads compete for IOPS and bandwidth:
  <https://learn.microsoft.com/en-us/troubleshoot/windows-server/networking/high-cpu-usage-issue-smb-server>
- SMB 3.1.1 directory caching reduces round trips but does not remove application-level
  queue contention:
  <https://learn.microsoft.com/en-us/windows-server/storage/file-server/smb-feature-descriptions>
- PhotoPrism exposes worker limits because large indexing and thumbnail workloads can
  otherwise overwhelm shared resources:
  <https://github.com/photoprism/photoprism/issues/497>
- LibVLC permits per-media read/cache options, but cache values affect startup, seek,
  and buffering differently and therefore require measurement:
  <https://videolan.videolan.me/vlc/master/group__libvlc__media.html>
- Current Facial source can create roughly half the logical CPU count in image workers,
  independently runs scans and metadata sweeps, and repeatedly resolves mapped paths
  while active video shares the same NAS.

## Selected approach

- Add a root-aware I/O coordinator with bounded classes for visible, playback,
  interactive metadata, prefetch, and bulk reconciliation work.
- Reserve capacity for visible work; throttle prefetch and bulk work while playback or
  direct interaction is active; release all permits on cancellation and failure.
- Use conservative remote-root concurrency and retain higher local-root throughput.
- Resolve and cache proven mapped-drive/UNC root identity once per configured root
  generation; derive child identities without repeated WNet calls.
- Record queue wait/depth, active class, cache hit, filesystem latency, player command
  and poll latency, and UI-frame stalls.
- Keep VLC cache bounded/configurable; move player calls to an actor thread only if
  timing proves synchronous calls violate the interaction budget.

## Rejected options

- Increasing workers until the link is saturated: queue depth can worsen interactive
  tail latency and playback control response.
- Suspending all background work during playback: it makes progress unpredictable and
  can leave visible thumbnails blank.
- A guessed universal VLC cache: NAS, codec, bitrate, and seek behavior vary.
- Treating a direct cable as the application fix: it cannot remove redundant work or
  provide scheduling priority.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-057" summary="Reserved interactive capacity and permit lifecycle tests prevent starvation, leaks, and local regressions." updated_at="2026-08-09">

## Risks, failures, and controls

- Playback starves scan forever: retain a small bounded bulk lane and resume its normal
  budget after playback/interaction hysteresis expires.
- Background work starves visible thumbnails: reserve visible capacity that bulk and
  prefetch work cannot consume.
- Cancellation leaks permits: use ownership-bound guards and test every success, error,
  cancellation, stale-generation, and panic boundary available to the worker API.
- Remote limits slow local SSD libraries: classify per root and preserve the existing
  local concurrency path unless measurements show a regression.
- Root identity cache survives remapping: bind it to configured root generation and
  invalidate it when the selected root or mapping proof changes.
- Playback cache improves streaming but harms seeking: benchmark start, representative
  seeks, stalls, and command latency before changing the default.

</topic>

<topic id="acceptance-plan" status="active" version="1" wp="WP-057" summary="Concurrent-load, route-observation, and negative-path probes gate release." updated_at="2026-08-09">

## Verification needs

- Deterministic coordinator tests for priority, fairness, bounds, cancellation, failure,
  and permit release.
- Concurrent scan, stat, thumbnail, scroll, playback, and seek scenarios with structured
  queue and latency evidence.
- Remote and local fixtures proving remote limits do not reduce local throughput.
- Root-remap and mapped/UNC alias tests proving one resolution per root generation and
  no unrelated-root collision.
- Cold/warm exact NAS runs, disconnect-mid-work recovery, and read-only SMB route
  observation after the direct link exists.
- Three consecutive full-suite passes, visual inspection, packaging, and adversarial
  review before completion.

</topic>
