---
file_id: REF-WP-088-CARGO-CONTAINMENT-V1
file_kind: refinement
updated_at: "2026-09-06"
---

<topic id="operator-request" status="active" version="1" wp="WP-088">

The operator approved `build-artifacts/cargo/` and the proposed enforcement: keep final/intermediate Cargo artifacts inside the repository and outside Git, reuse compilation within a WP, then clean after its final proof. Other projects are building concurrently; their processes, build roots, caches, and configuration must remain untouched. This packet settles build governance before continuing Match implementation; it changes no Match contract or status.

Existing anchors are CODEX sections 5.0.1 and 5.1, topology `path_contract.default_paths.cargo_build_dir`, and the README delivery rule. These previously selected `product/target/`. WP-088 replaces that active location. Product behavior in `specs/app-spec.md` is outside this governance-only refinement. The work packet is `governance/workpackets/wp-088-cargo-containment-v1.yaml`, linked from taskboard WP-088.

</topic>

<topic id="research-basis" status="active" version="1" wp="WP-088">

Sources checked on 2026-09-06: [Cargo configuration](https://doc.rust-lang.org/cargo/reference/config.html) and [Cargo build cache](https://doc.rust-lang.org/cargo/reference/build-cache.html). Cargo merges ancestor and Cargo-home configuration; environment and command-line settings can override repository defaults. Final output and intermediate output have distinct configuration keys. The cache documentation describes build artifacts and Cargo cleanup.

Selected approach: repository-local configuration plus one guarded PowerShell entrypoint pins both paths to `build-artifacts/cargo/`, supplies `build-artifacts/tmp/` to its child process, rejects conflicting overrides, and serializes Cargo/cleanup through an exclusive repository lock. Reuse the existing packaging and layout checks as callers, and retain the existing WP validation cadence. Two build jobs and serial tests bound concurrent demand. Config-only enforcement was rejected because overrides and invocation-directory discovery bypass it; global configuration was rejected because it affects unrelated projects. An operating-system sandbox is outside scope; raw Cargo remains able to bypass the guard.

Validation uses no full product rebuild: script negative cases plus a bounded real Cargo fixture prove the execution boundary. Independent review and governance parsing complete the contract proof. Shared Cargo dependency caches remain untouched; this policy contains build output, not downloaded dependencies.

</topic>

<topic id="red-team" status="active" version="1" wp="WP-088">

Override escape: reject output/config arguments and conflicting inherited settings before compilation, then pin effective settings. Wrong working directory: derive paths from the script location and test invocation outside the repository. Concurrent clean/build: acquire the same exclusive lock for both and fail safely on contention. Junction or symlink escape: reject reparse points in cleanup ancestry and descendants before deletion. Another process holding an artifact: report the exact blocker; never terminate it or silently claim cleanup. Stale legacy target: reject active old output routes and preserve uninspected artifacts pending exact ownership and cleanup authorization. Temporary directories left after failure: retain an actionable error and use the same guarded cleanup path after work ends. Full product tests add no proof of these script controls; use a real small Cargo fixture plus explicit failure cases.

</topic>

<topic id="implementation-and-proof" status="active" version="1" wp="WP-088">

Implement configuration and the guarded `-CargoArgs`, `-Probe`, and `-Clean` routes; route repository compilation callers and packaging through the guard; update executable lookup and invariant checks; synchronize CODEX, README, ignore rules, build rules, and topology. One coordinator runs all Cargo proof, while agents may implement and review independently. Complete script proof and real fixture placement/cleanup, run governance parsing, resolve independent-review findings, then record observed results in WP-088 and its taskboard row. Cleanup is complete only when the canonical scratch directories are absent. Match implementation resumes under the new guard after this predecessor passes.

</topic>
