//! Explorer file-operation helpers (WP-045): rename, new folder, cut/move.
//!
//! Pure-logic validation + careful filesystem verbs shared by the media
//! surface's cut/paste-move, rename, and new-folder actions. Every mutation
//! is per-file, verified, and reports precise errors; a failed copy NEVER
//! deletes its source.

use std::path::{Path, PathBuf};

/// Windows reserved device names (case-insensitive, extension-independent).
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Validate a single file/folder name (not a path) for Windows rules:
/// non-empty, no path separators, no reserved characters, no reserved device
/// names, no trailing dot/space. Returns a human-readable rejection.
pub fn validate_name(name: &str) -> Result<(), String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("name is empty".to_string());
    }
    if trimmed.len() > 240 {
        return Err("name is too long".to_string());
    }
    for c in trimmed.chars() {
        if matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32 {
            return Err(format!("name contains a forbidden character: {c:?}"));
        }
    }
    if trimmed.ends_with('.') || trimmed.ends_with(' ') {
        return Err("name may not end with a dot or space".to_string());
    }
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        return Err(format!("'{stem}' is a reserved Windows name"));
    }
    Ok(())
}

/// Preserve the source extension unless the new name supplies one.
/// `rename_with_override(true)` uses the new name verbatim.
pub fn apply_extension_policy(source: &Path, new_name: &str, keep_extension: bool) -> String {
    if !keep_extension {
        return new_name.to_string();
    }
    let new_has_ext = Path::new(new_name).extension().is_some();
    if new_has_ext {
        return new_name.to_string();
    }
    match source.extension().and_then(|e| e.to_str()) {
        Some(ext) if !ext.is_empty() => format!("{new_name}.{ext}"),
        _ => new_name.to_string(),
    }
}

/// First free `name`, `name (2)`, `name (3)`, … target in `dir`
/// (extension preserved: `photo (2).jpg`).
pub fn unique_target(dir: &Path, file_name: &str) -> PathBuf {
    let candidate = dir.join(file_name);
    if !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file_name);
    let ext = Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    for n in 2..10_000 {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem} (overflow){ext}"))
}

/// Rename `source` to `new_name` inside its own folder. Refuses collisions
/// (no silent overwrite) and invalid names. Returns the new path.
pub fn rename_file(source: &Path, new_name: &str, keep_extension: bool) -> Result<PathBuf, String> {
    let final_name = apply_extension_policy(source, new_name.trim(), keep_extension);
    validate_name(&final_name)?;
    let final_name = final_name.trim().to_string();
    let dir = source
        .parent()
        .ok_or_else(|| "source has no parent folder".to_string())?;
    let target = dir.join(&final_name);
    if target == source {
        return Ok(target);
    }
    // Same-file case-only rename is allowed on Windows; a different existing
    // file is a collision.
    let case_only = target
        .to_string_lossy()
        .eq_ignore_ascii_case(&source.to_string_lossy());
    if target.exists() && !case_only {
        return Err(format!("'{final_name}' already exists"));
    }
    std::fs::rename(source, &target).map_err(|e| format!("rename failed: {e}"))?;
    Ok(target)
}

/// Create `name` inside `dir`. Refuses invalid names and collisions.
pub fn create_folder(dir: &Path, name: &str) -> Result<PathBuf, String> {
    validate_name(name)?;
    let target = dir.join(name.trim());
    if target.exists() {
        return Err(format!("'{}' already exists", name.trim()));
    }
    std::fs::create_dir(&target).map_err(|e| format!("create folder failed: {e}"))?;
    Ok(target)
}

/// Move one file into `dest_dir` (auto-uniqued name). Fast path: rename.
/// Cross-volume fallback: copy, verify length, then delete the source —
/// the source is NEVER deleted on a failed or short copy.
pub fn move_file(source: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let file_name = source
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "source has no file name".to_string())?;
    if !dest_dir.is_dir() {
        return Err(format!(
            "destination is not a folder: {}",
            dest_dir.display()
        ));
    }
    if source.parent() == Some(dest_dir) {
        return Ok(source.to_path_buf()); // moving into its own folder: no-op
    }
    let target = unique_target(dest_dir, file_name);
    match std::fs::rename(source, &target) {
        Ok(()) => Ok(target),
        Err(_) => {
            // Cross-volume: copy + verify + delete.
            let src_len = std::fs::metadata(source)
                .map_err(|e| format!("stat source: {e}"))?
                .len();
            std::fs::copy(source, &target).map_err(|e| format!("copy failed: {e}"))?;
            let dst_len = std::fs::metadata(&target)
                .map_err(|e| format!("stat copy: {e}"))?
                .len();
            if dst_len != src_len {
                let _ = std::fs::remove_file(&target);
                return Err(format!(
                    "copy verification failed ({dst_len} of {src_len} bytes) — source kept"
                ));
            }
            std::fs::remove_file(source)
                .map_err(|e| format!("copied, but could not remove source: {e}"))?;
            Ok(target)
        }
    }
}

/// Batch move with per-file error report: `(moved, failures)`.
pub fn move_files(sources: &[String], dest_dir: &Path) -> (Vec<PathBuf>, Vec<(String, String)>) {
    let mut moved = Vec::new();
    let mut failures = Vec::new();
    for source in sources {
        match move_file(Path::new(source), dest_dir) {
            Ok(target) => moved.push(target),
            Err(err) => failures.push((source.clone(), err)),
        }
    }
    (moved, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("facial-fs-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn windows_name_validation() {
        assert!(validate_name("photo.jpg").is_ok());
        assert!(validate_name("my folder").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("  ").is_err());
        assert!(validate_name("a/b").is_err());
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("trailing.").is_err());
        // Leading/trailing whitespace is auto-trimmed by the create/rename
        // paths (Explorer behavior), so it is valid input here.
        assert!(validate_name("trailing ").is_ok());
        assert!(validate_name("CON").is_err());
        assert!(
            validate_name("con.txt").is_err(),
            "reserved stem with extension"
        );
        assert!(validate_name("lpt3.jpg").is_err());
        assert!(
            validate_name("console.txt").is_ok(),
            "prefix is not reserved"
        );
    }

    #[test]
    fn extension_policy_preserves_unless_overridden() {
        let src = Path::new("d:/x/photo.jpg");
        assert_eq!(apply_extension_policy(src, "hero", true), "hero.jpg");
        assert_eq!(apply_extension_policy(src, "hero.png", true), "hero.png");
        assert_eq!(apply_extension_policy(src, "hero", false), "hero");
    }

    #[test]
    fn unique_target_counts_up() {
        let dir = temp_dir("unique");
        std::fs::write(dir.join("a.jpg"), b"x").unwrap();
        std::fs::write(dir.join("a (2).jpg"), b"x").unwrap();
        let target = unique_target(&dir, "a.jpg");
        assert_eq!(target.file_name().unwrap().to_str().unwrap(), "a (3).jpg");
        let fresh = unique_target(&dir, "b.jpg");
        assert_eq!(fresh.file_name().unwrap().to_str().unwrap(), "b.jpg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rename_refuses_collision_and_preserves_extension() {
        let dir = temp_dir("rename");
        let a = dir.join("a.jpg");
        std::fs::write(&a, b"aaa").unwrap();
        std::fs::write(dir.join("taken.jpg"), b"bbb").unwrap();
        assert!(rename_file(&a, "taken", true).is_err(), "collision refused");
        assert!(a.exists(), "source untouched after refusal");
        let renamed = rename_file(&a, "fresh", true).unwrap();
        assert_eq!(renamed.file_name().unwrap().to_str().unwrap(), "fresh.jpg");
        assert!(renamed.exists() && !a.exists());
        assert!(rename_file(&renamed, "bad/name", true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_folder_validates_and_refuses_existing() {
        let dir = temp_dir("newfolder");
        let made = create_folder(&dir, "shoots").unwrap();
        assert!(made.is_dir());
        assert!(create_folder(&dir, "shoots").is_err());
        assert!(create_folder(&dir, "NUL").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_file_renames_and_uniquifies() {
        let dir = temp_dir("move");
        let src_dir = dir.join("from");
        let dst_dir = dir.join("to");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::create_dir_all(&dst_dir).unwrap();
        let a = src_dir.join("a.jpg");
        std::fs::write(&a, b"payload").unwrap();
        std::fs::write(dst_dir.join("a.jpg"), b"other").unwrap();
        let moved = move_file(&a, &dst_dir).unwrap();
        assert_eq!(moved.file_name().unwrap().to_str().unwrap(), "a (2).jpg");
        assert!(!a.exists());
        assert_eq!(std::fs::read(&moved).unwrap(), b"payload");
        // Move into own folder is a no-op.
        let b = dst_dir.join("b.jpg");
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(move_file(&b, &dst_dir).unwrap(), b);
        assert!(b.exists());
        // Missing destination errors, source kept.
        let c = dst_dir.join("c.jpg");
        std::fs::write(&c, b"x").unwrap();
        assert!(move_file(&c, &dir.join("missing")).is_err());
        assert!(c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn move_files_reports_per_file_failures() {
        let dir = temp_dir("batch");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&dst).unwrap();
        let ok = dir.join("ok.jpg");
        std::fs::write(&ok, b"x").unwrap();
        let missing = dir.join("missing.jpg").to_string_lossy().to_string();
        let (moved, failures) =
            move_files(&[ok.to_string_lossy().to_string(), missing.clone()], &dst);
        assert_eq!(moved.len(), 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, missing);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
