---
file_id: REF-WP-071-MANUAL-SPEC-TOPOLOGY-SYNC-V1
file_kind: refinement
updated_at: "2026-08-12"
---

<topic id="operator-request" status="active" version="1" wp="WP-071" summary="Bring the built-in Manual, the specification, and the topology back into agreement with the shipped product code after WP-064 through WP-070." updated_at="2026-08-12">

## Operator request

Verbatim operator item folded into this packet:

- "update internal user manual to reflect product code."

## Interpretation

- "Internal user manual" is `product/docs/MANUAL.md`, the in-app Manual surface required by
  `CODEX.md` section 6 and `governance/build_rules.yaml` FACIAL-MODEL-004.
- The requirement is agreement with the **shipped product code**, so this packet runs last,
  after WP-064 through WP-070 land, and documents what actually exists rather than what was
  planned.
- Because `CODEX.md` section 6 requires the Manual to be mirrored in `specs/app-spec.md`, and
  `[GLOBAL-BUILD-083]` treats spec drift as a project-quality defect, the spec and topology are
  synchronized in the same pass.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-071" summary="The Manual already has the right structure and a media chapter; known drift exists in a prior refinement and in surfaces that WP-064..WP-070 change." updated_at="2026-08-12">

## Baseline project evidence (inspected, current HEAD)

- `product/docs/MANUAL.md` is 1,522 lines with an operator guide and a reference part. The
  Media chapter spans `product/docs/MANUAL.md:143-446`, with subsections: What it is (145),
  Browsing (175), Playing videos (236), Selecting and acting on files (280), Labels, tags and
  notes (302), Search (333), Favorites and panels (357), Controller (378), Hiding the interface
  (412), and Large-folder diagnostics and recovery (421).
- Reference sections that this work touches: Media browser automation
  (`product/docs/MANUAL.md:1604`), storage layout (`:1608`), headless commands (`:1623`),
  UI-intents (`:1656`), failure modes (`:1720`), and the GUI inspector (`:1734`).
- `specs/app-spec.md` is 996 lines and records per-work-packet sections appended in order, most
  recently `### WP-063 …` at `specs/app-spec.md:1028-1077`. Section 15
  (`specs/app-spec.md:696`) holds the media browser contract.
- `topology.yaml` carries the machine-readable contracts this work changes:
  `media_browser` (`topology.yaml:278-375`), including `modules` (288-300), `state_paths`
  (301-307), `commands` (308-318), `search` (319-325), `labels` (326-331),
  `interaction_contract` (345-355), `performance_contract` (356-374), and
  `inspector_presets` (375).
- **Known drift already identified during this work:**
  `governance/refinements/wp-060-embedded-video-panel-terminology-v1.md:31` states the video
  parent HWND is discovered via `GetActiveWindow`. That is incorrect against current code — the
  parent is bound from the exact eframe handle at `product/src/ui.rs:930-939` and made immutable
  at `product/src/video_player.rs:338-341`; the only `GetActiveWindow` call is in
  `open_with_dialog` (`product/src/video_player.rs:1066`). WP-065 corrects this record.
- **Known drift the Manual will need to absorb** once WP-064..WP-070 land: favorites move from a
  right-edge overlay to a collection tab (WP-067), the sort key set gains created-time and
  becomes per tab (WP-068), search gains activation, scope, and subtractive filters (WP-066),
  the tab schema gains a kind discriminant (WP-067), load order becomes layered (WP-069), and
  font coverage depends on optional Windows system faces (WP-070).
- `topology.yaml:331` declares `render_db_calls: forbidden` with no enforcement in code; WP-069
  makes it enforceable, which changes what the topology can honestly claim.
- The Manual's own contract requires it to be sufficient for a model with **no prior context**
  (`CODEX.md:76`, `governance/build_rules.yaml` FACIAL-MODEL-004), covering executable roles,
  installed data paths, Media tab workflows, staged-versus-committed folder behavior,
  shortcuts, model intents, diagnostics, common failures, and recovery.

## Current external sources checked

- Diátaxis, the widely adopted documentation framework separating tutorial, how-to, reference,
  and explanation — the Manual already follows this split between its operator guide and
  reference part, and the update should preserve it: <https://diataxis.fr/>
- Google developer documentation style guidance on task-based instructions and documenting
  actual behavior rather than intent:
  <https://developers.google.com/style/procedures>
- Write the Docs guidance on docs-as-code and keeping documentation changes in the same change
  set as the code they describe: <https://www.writethedocs.org/guide/docs-as-code/>
- Microsoft guidance on documenting keyboard shortcuts and accessibility affordances, relevant
  to the shortcut tables the Manual carries:
  <https://learn.microsoft.com/en-us/windows/apps/design/input/keyboard-accelerators>
- Current agent-documentation practice for machine-operable products is to give every operator
  action a structured, receipt-backed equivalent and to document the probe that proves it —
  which is what `governance/build_rules.yaml` FACIAL-MODEL-001..003 already require.
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no further directly
  transferable guidance for this item.

## Selected approach

- Run this packet **last**, against shipped code, and verify each documented claim against the
  implementation rather than against the preceding packets' intent.
- Update the Manual's Media chapter section by section for the surfaces WP-064..WP-070 changed:
  tabs and tab kinds, folder staged-versus-committed navigation and its recovery, playback
  placement and its diagnostics, search activation/scope/filters, favorites and labels as a
  collection tab, per-tab ordering, load-order layering, and font coverage with its stated
  limits.
- Update the reference part: new and changed model intents and their receipt fields, new
  diagnostics and trace phases, new inspector presets, and new failure modes with recovery
  steps.
- Append a `### WP-064..WP-071` section to `specs/app-spec.md` in the established per-packet
  style, and correct any statement in section 15 or the WP-050..WP-063 sections that the new
  work invalidates rather than leaving contradictory text in place.
- Update `topology.yaml`: module list, commands and intents, `interaction_contract`,
  `performance_contract`, `inspector_presets`, tab state paths and schema version, and the
  search/labels contracts.
- Update `CODEX.md` section 8.1 and `README.md` where the media navigation and playback contract
  statements change.
- Correct the stale `GetActiveWindow` claim in the WP-060 refinement.
- Prefer removing or rewriting a wrong statement over appending a newer one beside it, so the
  documents do not accumulate contradictions.

## Rejected options

- Writing the documentation before or during implementation: it would document intent, and the
  operator asked for the Manual to reflect product code.
- Documenting only the Manual and leaving spec and topology behind: `CODEX.md` section 6
  requires the mirror, and `[GLOBAL-BUILD-083]` treats spec drift as a defect.
- Appending a change-log section instead of correcting the affected passages: a no-context model
  reading the Media chapter would still find the wrong instructions.
- Restructuring the Manual: the existing two-part structure works and matches the Diátaxis split;
  restructuring is unrequested scope and would churn every anchor.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-071" summary="Documentation asserting untested behavior, contradictions left in place, and broken in-app navigation have explicit controls and a no-context operability proof gate." updated_at="2026-08-12">

## Risks, failure scenarios, and controls

- **The Manual documents intended rather than shipped behavior.** Control: every documented
  command, intent, path, shortcut, and receipt field is executed or inspected against the built
  binary before it is written; unverified statements are not written.
- **Contradictions survive in older spec sections.** Control: search the spec and Manual for
  each superseded claim (favorites overlay, three sort keys, four filter chips, per-frame
  placement) and correct each occurrence, not just the newest section.
- **Manual in-app navigation breaks.** The Manual is rendered in a two-pane navigator
  (WP-027). Control: run the Manual tab through `ui-inspect` and confirm the contents tree,
  headings, and scrolling still work after the edit.
- **Documented shortcuts drift from the actual bindings.** Control: cross-check every documented
  key against the bindings table and the action vocabulary in `product/src/media_input.rs`.
- **Documented CLI examples do not run.** Control: execute each documented `facial-cli`
  invocation and paste real output shapes, including a failure case.
- **A no-context model still cannot operate a new surface.** This is the actual acceptance
  bar. Control: the proof is a fresh-context model driving the changed surfaces using only the
  Manual, with no conversation history — per `governance/build_rules.yaml` FACIAL-MODEL-003, a
  gap here means the missing intent, diagnostic, or fixture is added, not merely described.
- **Topology claims something the code does not enforce.** The current
  `render_db_calls: forbidden` line is exactly this failure. Control: topology statements must
  name their enforcement, or be phrased as intent rather than guarantee.
- **Font support is overstated.** Control: the Manual states monochrome emoji only, optional
  system-font dependency, and any Thai shaping limitation, matching WP-070's measured outcome.
- **Documentation work is treated as progress while product defects remain.** Control: this
  packet cannot start until WP-064..WP-070 are implemented and verified.

## Verification

- Execute every documented `facial-cli` command and intent against the built binary and compare
  the real receipt fields to the documented ones.
- Cross-check every documented shortcut against `product/src/media_input.rs` bindings.
- Render the Manual tab through `facial-cli ui-inspect` and directly inspect the PNG and layout
  JSON for the contents tree, heading hierarchy, and overflow.
- Full `cargo test --manifest-path product/Cargo.toml`.
- No-context operability check: a fresh-context model performs the changed workflows — open a
  folder in a new tab, recover a stuck modal, play in Library and in Viewer, search with a
  subtractive filter in folder-only scope, open the favorites collection tab, change sort to
  created-time descending — using only the Manual.
- Confirm `product/docs/MANUAL.md`, `specs/app-spec.md`, `topology.yaml`, `CODEX.md`,
  `README.md`, `governance/taskboard.yaml`, and the WP-064..WP-070 packet statuses agree, with
  no contradictory statements remaining.
- Independent high-risk adversarial review specifically hunting for documented-but-false claims.

</topic>
