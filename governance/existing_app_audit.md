---
file_id: existing_app_audit
file_kind: inspection-note
updated_at: 2026-06-08
---

<topic id="app-source-fusion-decision" status="completed" version="0.2" owner="model" updated_at="2026-06-08" ingestable="true">

## Scope
Inventory and feature transfer scope was completed before implementation, with the operator confirming full-feature fusion across candidate source apps.

## What was inspected
The source apps were inspected in `_source_checks/` and matched to canonical repositories.
Local references are:
`_source_checks/facet`, `_source_checks/python-ofiq`, `_source_checks/deepface`, `_source_checks/imagededup`, and `_source_checks/eDifFIQA`.

## Findings
The combined app will copy all feature domains from all five candidate apps, not only headshot culling.
`facet` contributes local end-to-end quality pipelines, duplicate workflows, burst/blink detection, and web UI flow.
`python-ofiq` contributes explicit face IQ scalar/vector scoring with face quality dimensions.
`deepface` contributes detection, embeddings, verification, search, registration, and database index flows.
`imagededup` contributes perceptual hashing, CNN duplicate search, and duplicate removal recommendations.
`eDifFIQA` contributes additional face IQ model families, model-variant switching, and batch score exports.

The source registry artifact is:
`governance/source_app_registry.yaml`

## Merge implications
Feature overlap exists across duplicate and quality scoring surfaces.
The merged app will resolve overlap through a normalized pass registry, configurable scorer graph, and explicit source attribution per score.

## Rust implementation note
- Runtime feature execution is implemented natively under `product/src/plugins/*.rs`.
- Python adapter modules and legacy backend/frontend shims were removed from active runtime paths.
- Priority for any future parity work is to keep Rust feature execution deterministic and keep manifests as source-of-truth descriptors.

## Source registry snapshot

```yaml
sources:
  facet:
    local: _source_checks/facet
    canonical: https://github.com/ncoevoet/facet
  python-ofiq:
    local: _source_checks/python-ofiq
    canonical: https://github.com/unicef/python-ofiq
  deepface:
    local: _source_checks/deepface
    canonical: https://github.com/serengil/deepface
  imagededup:
    local: _source_checks/imagededup
    canonical: https://github.com/idealo/imagededup
  ediffiqa:
    local: _source_checks/eDifFIQA
    canonical: https://huggingface.co/opencv/face_image_quality_assessment_ediffiqa
```

</topic>

<topic id="merge-plan-v1" status="open" version="0.1" owner="model" updated_at="2026-06-08">

## Merge surface plan (phase 1)
Build one plugin descriptor per source app in `product/plugins/`.
Route all features into a shared registry and debug event stream.
Keep default image behavior copy-safe unless operators explicitly enable in-place mode.

## Merge surface plan (phase 2)
Run a feature-by-feature parity pass against each source app area.
Capture remaining gaps and update this artifact before implementation of each pass.

</topic>

<topic id="runtime-rust-parity-contract-v2" status="completed" version="0.1" owner="model" updated_at="2026-06-08" ingestable="true">

## Runtime parity contract after Rust rewrite
The runtime now implements deterministic feature outputs directly in Rust under `product/src/plugins`.

## Implemented source feature mapping
- `facet` -> `product/src/plugins/facet.rs`
  - `quality_pass`, `composition_pass`, `faces_pass`, `duplicate_pass`, `burst_blink_pass`, `diagnostics_pass`
  - duplicate policy preserved as thresholded hash/embedding group scoring
  - `duplicate_pass` outputs `group_key`, `member_files`, `avg_similarity`, `min_similarity`, `max_similarity`, and `coverage_percent`
- `python-ofiq` -> `product/src/plugins/python_ofiq.rs`
  - `setup_data`, `scalar_quality`, `vector_quality`
  - vector schema is deterministic and versioned (`schema.version: 0.2-native`)
- `deepface` -> `product/src/plugins/deepface.rs`
  - `detect`, `analyze`, `represent`, `register`, `find`, `verify`
  - identity outputs include explicit `decision` fields and numeric thresholds
- `imagededup` -> `product/src/plugins/imagededup.rs`
  - `hash_duplicates`, `cnn_duplicates`, `remove_candidates`
  - removal policy outputs connected-component keep/remove reasoning
- `eDifFIQA` -> `product/src/plugins/ediffiqa.rs`
  - `model_t`, `model_m`, `model_s`, `model_l`, `batch_inference`
  - batch mode includes `winner_counts` and `winner_score_stats`

## Artifact locations to use when continuing implementation
- run root: `runs/<run_id>/<plugin>/<feature>/`
- per-feature file: `<artifact>.json` written by `write_feature_artifact`
- run summary: `runs/<run_id>/results.json`

## Governance and continuation notes
- Feature contracts are now source-of-trust in:
  - `governance/workpackets/wp-006-plugin-parity-spec.yaml`
  - `specs/app-spec.md`
- Next update step after runtime changes is to re-run governance review of the same artifact set when behavior is adjusted.

</topic>
