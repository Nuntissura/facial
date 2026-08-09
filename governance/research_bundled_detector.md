---
file_id: research_bundled_detector
file_kind: research
updated_at: 2026-06-11
---

# Research: Bundled Default Face Detector (YuNet) — Sourcing and License Verification

<topic id="scope" wp="WP-020" summary="What this research covers and does not cover">

This research closes the sourcing and license-verification questions of WP-020
(`governance/workpackets/wp-020-bundled-detector.yaml`, research_questions: license,
distribution artifact, version pinning). It covers:

- Verifying the license of the OpenCV Zoo YuNet model file `face_detection_yunet_2023mar.onnx`
  and whether redistribution/bundling inside a Rust app binary or release package is permitted.
- Downloading and integrity-pinning the float (non-quantized) 2023mar model into
  `product/assets/models/`.
- Recording attribution duties and the license text shipped beside the model
  (`product/assets/models/YuNet-LICENSE.txt`).

Not covered here (remains WP-020 implementation work): detector resolution order in
`product/src/config.rs` (override -> bundled), packaging script changes, manifest/receipt
sha256 stamping, MANUAL/spec/topology updates, startup self-check.

</topic>

<topic id="license-verdict" wp="WP-020" summary="MIT; bundling permitted: YES; ship copyright+permission notice">

**License: MIT.** The model directory `models/face_detection_yunet/` in `opencv/opencv_zoo`
carries its own `LICENSE` file: MIT License, `Copyright (c) 2020 Shiqi Yu <shiqi.yu@gmail.com>`.
The directory README states verbatim: "All files in this directory are licensed under MIT
License." — this explicitly covers the `.onnx` model files in that directory, including
`face_detection_yunet_2023mar.onnx`.

**Bundling/redistribution permitted: YES.** MIT grants the rights "to use, copy, modify,
merge, publish, distribute, sublicense, and/or sell copies of the Software" without
restriction. Embedding the model bytes in the binary, shipping it beside the exe, and
auto-downloading are all permitted.

**Attribution duties (binding):** the copyright notice and permission notice "shall be
included in all copies or substantial portions of the Software." Satisfied by shipping
`product/assets/models/YuNet-LICENSE.txt` (verbatim license + source URL + retrieval date)
alongside the model in every distribution that contains the model bytes. If the model is
ever embedded into the executable via `include_bytes!`, the license text must still ship
with the release package (or be embedded in a third-party-notices surface).

**Non-binding courtesy:** the README requests an academic citation of the YuNet paper
(`wu2023yunet`, Machine Intelligence Research 20(5), 2023). This is a request, not a
license condition; recorded in `sources` below.

**Warranty:** software is provided "AS IS" with no warranty — consistent with the WP-020
red-team control that a startup smoke-detect self-check (not the license) gates
`source: real`.

</topic>

<topic id="artifact" wp="WP-020" summary="232,589 bytes; sha256 8f2383e4...2552fa4; float 2023mar export" ingestable="true">

Downloaded 2026-06-11 via PowerShell `Invoke-WebRequest`.

```yaml
artifact:
  path: product/assets/models/face_detection_yunet_2023mar.onnx
  byte_size: 232589
  sha256: 8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4
  source_url: https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx
  source_repo: opencv/opencv_zoo (branch main)
  model_version: 2023mar (float / non-quantized, fixed-input-shape export)
  license_file: product/assets/models/YuNet-LICENSE.txt
  retrieved_at: 2026-06-11
```

Integrity verification performed:

- `opencv_zoo` stores models in git-LFS. The plain raw URL
  (`raw.githubusercontent.com/.../face_detection_yunet_2023mar.onnx`) serves a ~130-byte
  LFS **pointer file**, not the model. The pointer declares
  `oid sha256:8f2383e4dd3cfbb4553ea8718107fc0423210dc964f9f4280604804ed2552fa4` and
  `size 232589`.
- The downloaded artifact matches the repo-declared LFS oid and size **exactly**
  (`Get-FileHash -Algorithm SHA256`), proving it is the real float model from `main`,
  not a pointer file and not an int8 variant (`_int8.onnx` / `_int8bq.onnx` are separate
  files with different oids).
- File header bytes are valid ONNX protobuf (ir_version 6, producer `pytorch`).
- Size note: the float 2023mar export is 232,589 bytes (~227 KiB). The "~345 KB" figure
  sometimes quoted for YuNet corresponds to the older 2022mar export; 232,589 bytes is the
  correct, repo-pinned size for the 2023mar float model and is inside the expected
  200–500 KB envelope. Distribution-cost question from WP-020 ("+~300KB ok?") is therefore
  answered: actual cost is ~227 KiB.

</topic>

<topic id="version-pinning" wp="WP-020" summary="2023mar is the only export matching identity.rs FaceDetectorYN-compatible decode">

`product/src/identity.rs` implements a FaceDetectorYN-compatible decode written against the
**OpenCV Zoo 2023mar** output layout (per the module's own decode comments, confirmed there
via output shapes): 12 output tensors over 3 strides [8, 16, 32], 1 anchor per cell, output
groups `cls[0..3]`, `obj[3..6]`, `bbox[6..9]`, `kps[9..12]`; linear cx/cy decode,
**exponential** w/h decode (`exp(dw)*stride`), landmark decode `(col/row + delta)*stride`;
input is raw 0–255 BGR at a fixed square `DET_INPUT`; greedy NMS uses the OpenCV
FaceDetectorYN default IoU cutoff.

Why pin exactly `2023mar` (float):

- The 2023mar export is a fixed-input-shape model — matching the fixed `DET_INPUT`
  square-grid assumption in the decode (`grid = sqrt(rows)`, `stride = DET_INPUT / grid`).
- The newer `face_detection_yunet_2026may.onnx` switched to dynamic (symbolic) H/W input
  dims to support the OpenCV 5.x ONNX Runtime engine — a different input contract than the
  decode assumes; adopting it would require decode-path rework and revalidation.
- Older exports (2022mar) predate the layout the decode was written and verified against.
- Quantized variants (`_int8`, `_int8bq`) are different artifacts with different hashes;
  the WP-020 manifest/receipt stamping requires a single canonical sha256, recorded in
  the `artifact` topic above.

Pinning contract: the bundled artifact is identified by the sha256 above; any future model
bump (e.g. to 2026may) is a deliberate WP with decode changes + smoke-detect revalidation,
never a silent file swap (WP-020 red-team scenario: bundled model silently differs from
decode expectations).

</topic>

<topic id="sources" wp="WP-020" summary="URLs checked on 2026-06-11">

All retrieved 2026-06-11:

- Model directory (file list, README license statement, citation request, 2023mar vs
  2026may notes): https://github.com/opencv/opencv_zoo/tree/main/models/face_detection_yunet
- LICENSE (verbatim MIT text, Copyright (c) 2020 Shiqi Yu):
  https://raw.githubusercontent.com/opencv/opencv_zoo/main/models/face_detection_yunet/LICENSE
- README raw: https://raw.githubusercontent.com/opencv/opencv_zoo/main/models/face_detection_yunet/README.md
- Git-LFS pointer (authoritative oid sha256 + size for the float model):
  https://raw.githubusercontent.com/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx
- Binary download (canonical direct URL, resolves the LFS object):
  https://media.githubusercontent.com/media/opencv/opencv_zoo/main/models/face_detection_yunet/face_detection_yunet_2023mar.onnx
- YuNet paper (citation requested by README, non-binding): Wu W., Peng H., Yu S.,
  "YuNet: A Tiny Millisecond-level Face Detector", Machine Intelligence Research 20(5),
  656–665, 2023, Springer.
- Local decode evidence: `product/src/identity.rs` (FaceDetectorYN-compatible decode,
  2023mar layout comments); `governance/workpackets/wp-020-bundled-detector.yaml`
  (research questions answered here).

</topic>
