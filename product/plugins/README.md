# Plugin folder

Plugin metadata describes runtime behavior sourced from the legacy feature domains.

- Each source app includes a `metadata.json` manifest.
- Runtime feature execution is implemented in Rust at `product/src/plugins/*.rs`.
- Supported metadata keys:
  - `id`
  - `name`
  - `package`
  - `adapter`
  - `features`
  - `description`
- The app scans plugin manifests at startup and dispatches to Rust executors.
