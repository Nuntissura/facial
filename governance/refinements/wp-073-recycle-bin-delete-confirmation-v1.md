---
file_id: REF-WP-073-RECYCLE-BIN-DELETE-CONFIRMATION-V1
file_kind: refinement
updated_at: "2026-08-15"
---

<topic id="operator-request" status="active" version="1" wp="WP-073" summary="Delete from the context menu or keyboard with a confirmation popup, landing files in the Windows Recycle Bin rather than a facial-owned trashcan." updated_at="2026-08-15">

## Operator request

Verbatim operator items folded into this packet (2026-08-15):

- "delete in right click or on the keyboard or i think it is backspace? or whatever it is called to
  remove a letter backwards. it should show a pop up windows for comfirmation. those file should end
  up in the trashcan (windows explorer) or perhaps facial should also own a trashcan? this feels
  messy though."
- (added mid-session, folded here because it is the same context-menu file-op surface): "open file
  location does not work, it does launch a windows explorer window but not the file in its parent
  folder"

## Interpretation

- Delete must be reachable from the right-click context menu and from the keyboard.
- A confirmation popup must precede every delete.
- Deleted files go to the Windows Recycle Bin. The operator floated a facial-owned trashcan and
  immediately judged it messy; the Windows Recycle Bin is the selected destination and a
  facial-owned trashcan is a rejected option, not a requirement.
- On the keyboard the operator guessed Backspace. Backspace is already the Media surface's
  parent-folder navigation (Explorer parity), so the delete key stays the dedicated `Delete` key,
  which is already bound. This mirrors Windows Explorer exactly and is called out for operator
  awareness rather than silently decided.

</topic>

<topic id="evidence-and-research" status="active" version="1" wp="WP-073" summary="Today's delete is fs::remove_file - permanent, unconfirmed, synchronous on the render thread - and a governance claim about confirmation semantics was never true; the trash crate (IFileOperation-based) is the field standard, and network roots have no Recycle Bin." updated_at="2026-08-15">

## Baseline project evidence (inspected, current working tree)

- The single delete implementation is `compare_lane_delete_selected` at
  `product/src/ui.rs:15392-15490`: `fs::remove_file(&path)` at `product/src/ui.rs:15412` -
  **permanent, no Recycle Bin, no trash crate anywhere in the repo**. It runs synchronously on the
  render thread the moment the request bit is set, with **no confirmation dialog of any kind**.
  Folders are silently skipped because only `remove_file` is used. Post-delete it rebuilds the file
  list and clears the whole selection (`product/src/ui.rs:15440-15445`).
- Triggers today: Media/Compare context menu Delete (`product/src/ui.rs:2725-2734`, rendered in
  `theme::error_ink()`; Compare at `:2818`), Media action map `A::Delete` bound to the bare `Delete`
  key (`product/src/media_input.rs:397`, dispatched `product/src/ui.rs:12692`), and Compare-tab raw
  `delete_key || backspace_key` handling (`product/src/ui.rs:6856-6857`, `:6917-6919`). Dispatch
  `product/src/ui.rs:15594-15596`.
- **Backspace is taken**: `A::FolderUp -> Backspace` at `product/src/media_input.rs:386` (matching
  Explorer's Backspace-goes-up convention); binding it to delete would conflict with daily
  navigation. Bindings persist as `media_bindings_v1` version 7
  (`product/src/media_input.rs:17-18`), with forward-merge migration at
  `product/src/media_input.rs:472-545` if any binding changes.
- The only confirmation-modal precedent is the color-label definition delete confirm
  (`product/src/ui.rs:1293`, `:11986-12073`), which follows the shared modal shell rules
  (FACIAL-STYLE-001..003) and is the pattern to copy for a file-delete confirmation.
- Off-thread file-mutation precedent to copy: the cut/paste move path spawns a thread and reports
  back via `CompareWorkEvent::MediaMoveDone` (`product/src/ui.rs:7376-7393`, worker
  `product/src/ui.rs:16922-16938`, completion handler `product/src/ui.rs:3894-3940`).
- Governance honesty note: `governance/workpackets/wp-044-book-explorer-rebuild.yaml:79` claims
  "Delete from grid keeps existing non-destructive confirmation semantics from WP-032 plumbing" -
  **the code has never had confirmation or non-destructive semantics**; WP-032 only specified
  "Delete removes". This packet corrects the behavior; the false prior claim is recorded here so it
  is not cited as a baseline.
- Data-safety authority: CODEX section 8 requires default non-destructive behavior; an unconfirmed
  permanent delete reachable from one bare keypress on the operator's canonical 146k-file NAS
  folders is the single most dangerous surface in the app today.
- Metadata rows (notes/tags/labels/favorites) are keyed by path in `media.redb`
  (`product/src/media_db.rs:31-42`); a file restored from the Recycle Bin to the same path finds its
  metadata again, so metadata must NOT be deleted with the file.
- **Open-file-location defect (operator-reported, root-caused)**: `open_in_file_manager_with_system_app`
  at `product/src/ui.rs:2898-2903` passes `explorer` the switch and the path as TWO separate
  arguments (`command.args(["/select,", path])`). Explorer's `/select` works only when switch and
  path form ONE argument (`/select,C:\...\file.jpg`); split apart, Explorer ignores the selection
  and opens a default window - exactly the reported symptom. The field-hardened fix is the shell
  API `SHOpenFolderAndSelectItems` (exact reveal, immune to spaces/commas/Unicode), with the
  single-argument `/select,` form retained as fallback:
  <https://learn.microsoft.com/en-us/windows/win32/api/shlobj_core/nf-shlobj_core-shopenfolderandselectitems>
  (CoInitialize required; single-file usage passes the file's own PIDL as `pidlFolder` with
  `cidl=0`; free with `ILFree`).

## Current external sources checked

- `trash` crate v5.2.6 (2026-08-12, actively maintained): cross-platform Recycle Bin/Trash moves,
  `delete`/`delete_all`, Windows implementation over the modern shell operation API:
  <https://docs.rs/trash>, <https://crates.io/crates/trash>
- Microsoft `SHFileOperationW` documentation: permanent delete unless `FOF_ALLOWUNDO`; superseded by
  `IFileOperation` since Vista; the legacy API carries MAX_PATH-class limitations relevant to deep
  NAS trees: <https://learn.microsoft.com/en-us/windows/win32/api/shellapi/nf-shellapi-shfileoperationa>
- Windows behavior on network locations: **network/UNC paths have no Recycle Bin; deletes there are
  permanent** (Explorer itself warns and permanently deletes). Field references:
  <https://github.com/rstudio/rstudio/issues/17662>,
  <https://services.ncl.ac.uk/itservice/core-services/filestore/filestore-tips/thenetworkrecyclebin>
- Alternative crates surveyed: `ifop` (thin IFileOperation wrapper, low adoption), `rrecycle`,
  `recyclebin`: <https://docs.rs/ifop/latest/ifop/>, <https://crates.io/crates/rrecycle>
- Windows Explorer conventions (operator's reference implementation): Delete = recycle with
  optional confirm, Shift+Delete = permanent with distinct confirm, Backspace = navigate up.
- Hugging Face, Civitai, Reddit, and X/Twitter searches produced no additional transferable
  implementation evidence beyond the above.

## Selected approach

- Adopt the `trash` crate for the Recycle Bin move: field-hardened, actively maintained, built on
  the modern shell operation path (long-path aware), and the same crate the Rust desktop ecosystem
  standardized on. It adds the `windows` bindings crate to the tree alongside the existing
  `windows-sys`; both are pure-Rust bindings, so the Rust-native mandate (CODEX section 4) holds.
- One shared confirmation modal on the shell (FACIAL-STYLE-001..003) for every delete trigger
  (context menu and Delete key, Media and Compare surfaces), listing the exact count and up to a
  bounded sample of names, with the destructive action styled in `theme::error_ink()`.
- Root-aware messaging: before showing the modal, classify the selection's roots; for locations
  with no Recycle Bin (UNC/network, and any path where the trash operation is unsupported) the modal
  states plainly that the delete is PERMANENT for those files and requires the distinct
  permanent-delete confirmation. Root classification reuses the media_io root-kind knowledge and
  must not run filesystem calls inside the render span (guard test `product/src/ui.rs:16944-16978`).
- Execute deletes off-thread via the MediaMoveDone-style worker pattern with per-file failure
  reporting; on completion, remove deleted rows from the visible inventory/display without a rescan,
  keep metadata rows untouched, and surface a summary message naming successes and failures.
- Keyboard: `Delete` confirms-then-recycles; `Shift+Delete` opens the same modal in permanent mode
  (Explorer parity). Backspace remains FolderUp.
- Model route (FACIAL-MODEL-001): a receipt-backed delete intent that requires an explicit
  `--confirm` token, reports per-file outcome (`recycled | permanently_deleted | failed:<reason>`),
  and rejects without the token; receipts must describe state after the command per CODEX 8.3.
- Fix Open file location by replacing the split-argument explorer spawn with
  `SHOpenFolderAndSelectItems` on a short-lived worker thread (CoInitializeEx apartment-threaded,
  `ILCreateFromPathW` on the exact UTF-16 path, `ILFree`, `CoUninitialize`), falling back to a
  single-argument `explorer /select,<path>` spawn if the shell call fails. This matches the
  FACIAL-PLAYBACK-003 exact-UTF-16-path house rule and adds the `Win32_System_Com` windows-sys
  feature.

## Rejected options

- **Facial-owned trashcan** (operator's own verdict: messy): duplicates an OS facility, invents a
  second restore surface, writes clutter into NAS roots, and still needs its own retention policy.
  Rejected for local volumes outright; for network roots the honest permanent-delete warning beats a
  hidden `.facial-trash` folder on someone else's share.
- **Direct `SHFileOperationW` via the existing windows-sys binding** (zero new deps): legacy API
  with MAX_PATH-class path limits - a real hazard on deep NAS trees - and double-null path packing
  is easy to get subtly wrong. The maintained crate encapsulates the modern API.
- **`ifop` / hand-rolled COM `IFileOperation`**: more code and COM lifetime surface for no gain over
  the established crate.
- **Silent recycle without confirmation** (Explorer's modern default): rejected; the operator
  explicitly asked for a confirmation popup, and batch selections on NAS make mistakes expensive.
- **Deleting metadata rows with the file**: rejected; Recycle Bin restore would silently lose
  notes/tags/labels. Orphaned metadata is already the WP-067 remove-from-view model and harms
  nothing.

</topic>

<topic id="red-team-and-verification" status="active" version="1" wp="WP-073" summary="Permanent-delete honesty on network roots, modal single-flight, off-thread execution, folder rows, and receipt truthfulness are the failure surfaces; every one has an explicit control and proof gate." updated_at="2026-08-15">

## Risks, failure scenarios, and controls

- **A network-root file is presented as recyclable and is permanently lost.** Control: root
  classification decides the modal wording per file; the mixed-selection modal splits counts into
  recyclable vs permanent; a focused test covers UNC, mapped-network, and local fixtures; the
  runtime probe on the operator's real mapped root is part of acceptance.
- **The trash operation fails midway through a batch.** Control: per-file outcomes collected
  off-thread; partial success reported with exact failed names and reasons; no silent aggregate
  success (GLOBAL-SOT-027).
- **Delete key fires while a text field has focus.** Control: the existing keyboard focus gate
  (`product/src/ui.rs:12511-12513`) already suppresses keyboard actions during text editing; add a
  regression test so the modal cannot be summoned from notes editing.
- **Double-invocation while the modal is open or a worker is running.** Control: single-flight
  guard like the rename/new-folder arming pattern; the modal claims the top of its order layer per
  CODEX 8.3; repeated Delete presses while open are consumed by the modal.
- **A selected folder row reaches the file path.** Control: current behavior silently skips
  folders; the new path must state "folders are not deleted here" in the summary rather than
  silently skipping; deleting folders stays a non-goal.
- **The receipt claims recycled when the OS permanently deleted.** Control: outcome vocabulary is
  per-file and sourced from the actual operation branch, never inferred from the request; receipt
  honesty review per CODEX 8.3.
- **UI blocks on a slow NAS delete.** Control: worker thread with the MediaMoveDone pattern; the
  modal closes on dispatch and a busy status line reports progress; frame-time assertions in the
  existing ui_frame_diagnostics window during the live probe.
- **Inventory desync after delete.** Control: completion handler removes rows from the runtime
  inventory and display order for the affected tab only, mirroring the WP-067 remove-row path;
  a test asserts display_count drops by exactly the deleted count without a rescan.

## Verification

- Focused unit tests: confirmation state machine (open, confirm, cancel, single-flight), root
  classification split, per-file outcome aggregation, folder-skip messaging, focus-gate regression.
- Full `cargo test --manifest-path product/Cargo.toml`.
- Deterministic ui-inspect preset for the confirmation modal (default and permanent wording),
  asserting layering above the veil and unclipped action row; direct PNG/layout inspection.
- Live background proof on this workstation: recycle a scratch file on the local volume and verify
  it appears in the Windows Recycle Bin; attempt on the operator's mapped NAS root and verify the
  permanent-mode wording appeared and the receipt reported `permanently_deleted` (executed against
  disposable scratch files created for the proof, never operator media).
- Receipt-backed model intent proof under the live GUI database lock, including rejection without
  the confirm token.
- Independent high-risk adversarial review per FACIAL-VERIFY-004, focused on the destructive path.

</topic>
