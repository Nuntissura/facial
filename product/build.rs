use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=surrealdb-engine-compatibility.json");
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let lock_path = manifest_dir.join("Cargo.lock");
    let compatibility_path = manifest_dir.join("surrealdb-engine-compatibility.json");
    let raw = fs::read_to_string(&lock_path).expect("read product/Cargo.lock");
    let mut versions = raw
        .split("[[package]]")
        .filter_map(|block| {
            let mut name = None;
            let mut version = None;
            for line in block.lines().map(str::trim) {
                if let Some(value) = line
                    .strip_prefix("name = \"")
                    .and_then(|value| value.strip_suffix('"'))
                {
                    name = Some(value);
                }
                if let Some(value) = line
                    .strip_prefix("version = \"")
                    .and_then(|value| value.strip_suffix('"'))
                {
                    version = Some(value);
                }
            }
            (name == Some("surrealdb"))
                .then_some(version?)
                .filter(|value| !value.contains('-'))
        })
        .collect::<Vec<_>>();
    versions.sort_by_key(|value| {
        let mut parts = value
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0));
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    });
    let version = versions
        .last()
        .expect("Cargo.lock must contain a stable surrealdb package");
    let compatibility = fs::read_to_string(&compatibility_path)
        .expect("read product/surrealdb-engine-compatibility.json");
    let expected = compatibility
        .lines()
        .map(str::trim)
        .find_map(|line| {
            line.strip_prefix("\"engine_version\": \"")
                .and_then(|value| value.strip_suffix("\","))
        })
        .expect("compatibility manifest must contain engine_version");
    assert_eq!(
        version, &expected,
        "Cargo.lock SurrealDB version changed without a matching populated compatibility decision"
    );
    println!("cargo:rustc-env=FACIAL_SURREALDB_VERSION={version}");
}
