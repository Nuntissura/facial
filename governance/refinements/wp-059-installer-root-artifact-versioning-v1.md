---
file_id: wp-059-installer-root-artifact-versioning-v1
file_kind: refinement
updated_at: 2026-08-09
---

<topic id="operator-request" status="active" version="1" wp="WP-059" summary="Canonical delivery pair, archive, version bump, shortcuts, and launch">

## Operator request

- Keep exactly one current installer and one current portable EXE at the root of `installer/`.
- Move older installers and portable EXEs to `installer/installer-portable-archive/`.
- Increment the application version on every successful build.
- Ask during setup whether to create Desktop and Windows Start-menu shortcuts.
- Offer to launch Facial when setup completes.
- Change Codex/release authority and implementation without publishing a new build in this task.

## Authority anchors

- Supersedes the delivery-location portion of `CODEX.md` section 5.1 / WP-023.
- Extends `CODEX.md` section 5.2 / WP-025 without changing its install/data modes.
- Updates `topology.yaml`, `README.md`, `.gitignore`, release scripts, and Inno Setup source.

</topic>

<topic id="research-basis" status="complete" version="1" wp="WP-059" summary="Official Inno Setup and Microsoft Windows behavior">

## Sources checked

- [Inno Setup Tasks](https://jrsoftware.org/ishelp/topic_taskssection.htm): installer-visible checkbox tasks.
- [Inno Setup Components and Tasks parameters](https://jrsoftware.org/ishelp/topic_componentstasksparams.htm): bind Desktop/Start shortcuts to selected tasks.
- [Inno Setup Run section](https://jrsoftware.org/is6help/topic_runsection.htm): checked post-install launch, `skipifsilent`, and original-user execution.
- [Microsoft Windows 11 Start layout](https://learn.microsoft.com/en-us/windows-hardware/customize/desktop/customize-the-windows-11-start-menu): Pinned-grid customization belongs to OEM/managed-layout surfaces and user choice, not a normal desktop installer.
- Current project implementation: `installer/facial.iss`, `package-release.ps1`, `check-exe-layout.ps1`, Cargo manifest, CODEX, topology, and WP-023/WP-025.

## Selected approach

- Publish version-matched `facial-portable-X.Y.Z.exe` and `facial-setup-X.Y.Z.exe` at `installer/`.
- Increment the patch component transactionally; roll back version authority after pre-publication failure.
- Compile setup in transient `installer/payload/compiled/`, then archive/publish only after Cargo and ISCC both succeed.
- Use Inno `[Tasks]` for Desktop and Start-menu/All-apps shortcuts.
- Use a checked `[Run]` post-install action to launch under the original user; skip silent installs.

## Rejected options

- Force-pinning to the Windows Start Pinned grid: not a supported ordinary-installer contract and would override user-controlled layout.
- Keeping current output in `product/` plus a copied portable EXE in `installer/`: violates the single current delivery pair and creates ambiguous canonical artifacts.
- Bumping version only in the installer filename: leaves Cargo/topology version authority stale.

</topic>

<topic id="scope-and-red-team" status="active" version="1" wp="WP-059" summary="Scope edges, failure scenarios, controls, and verification">

## Scope

- Release artifact topology, versioning, migration/archive, setup shortcuts, post-install launch, authority, and static validation.

## Non-goals

- No new executable or installer is published by this work.
- No change to Update/Soft/Full/Uninstall data semantics.
- No forced Start-menu Pinned-grid mutation.

## Red team

- Failure: version increments but compilation fails. Control: retain exact manifest/topology/lock inputs and restore them before publication.
- Failure: installer compiles but portable build fails, or publication stops between the two root moves. Control: stage both artifacts first, record every archive move, and on any pre-validation failure remove partial new artifacts and restore archived files to their exact source paths.
- Failure: archive collision overwrites an older artifact. Control: collision-safe timestamp suffix; never overwrite archive entries.
- Failure: old output paths leave multiple current-looking binaries. Control: migrate legacy product/archive/out artifacts and enforce exactly two root EXEs.
- Failure: elevated setup launches Facial under the administrative account. Control: `postinstall` plus `runasoriginaluser`; silent installs skip launch.
- Failure: setup claims a Start pin that Windows ignores. Control: label the supported task as Start menu/All apps and document manual pinning.

## Verification

- PowerShell parser accepts both release scripts.
- Static SemVer probe proves `0.1.0 -> 0.1.1` and version replacement in Cargo/topology text.
- Inno source contains two shortcut tasks, task-bound icons, and checked post-install launch.
- Source/authority searches show no live canonical reference to `product/facial.exe` or `installer/out/` outside historical/superseded records.
- A later real packaging run must prove archive migration, `0.1.1` publication, ISCC success, scratch cleanup, and invariant exit 0.

</topic>
