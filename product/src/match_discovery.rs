//! Filesystem discovery state lives exclusively in the supervised Match child.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const MAX_TEXT: usize = 32 * 1024;
pub(crate) const CURSOR_BYTES: u64 = 2 * 1024 * 1024;
// Six-byte JSON escaping of 1 MiB exclusions, root and bounded fence envelope.
pub(crate) const BEGIN_REQUEST_BYTES: u64 = 8 * 1024 * 1024;
pub(crate) fn validate_begin(root: &Path, exclusions: &[String]) -> Result<(), String> {
    if root.as_os_str().len() > MAX_TEXT
        || exclusions.len() > 4096
        || exclusions.iter().any(|s| s.len() > MAX_TEXT)
        || exclusions.iter().map(String::len).sum::<usize>() > 1024 * 1024
    {
        return Err("Match discovery path or exclusion bound exceeded".into());
    }
    Ok(())
}

pub(crate) fn is_excluded(relative: &Path, exclusions: &[String]) -> bool {
    let candidate = relative
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    exclusions.iter().any(|excluded| {
        let excluded = excluded.trim_matches('/').to_ascii_lowercase();
        !excluded.is_empty()
            && (candidate == excluded || candidate.starts_with(&format!("{excluded}/")))
    })
}

fn bounded_error(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(2048).collect()
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    path: PathBuf,
    pub is_file: bool,
}
impl Entry {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Failure {
    path: Option<PathBuf>,
    message: String,
}
impl Failure {
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub(crate) type Step = Option<Result<Entry, Failure>>;

// Limits bound retained handles and paths independently of directory width.
const MAX_DEPTH: usize = 64;
const MAX_STATE_PATH_BYTES: usize = 256 * 1024;
struct Directory {
    path: PathBuf,
    entries: std::fs::ReadDir,
}
pub(crate) struct Discovery {
    root: PathBuf,
    exclusions: Vec<String>,
    stack: Vec<Directory>,
    pending: Option<PathBuf>,
    first: bool,
    current: Option<PathBuf>,
}
fn path_bytes(path: &Path) -> usize {
    path.as_os_str().len().saturating_mul(4)
}
fn failure(path: PathBuf, message: impl std::fmt::Display) -> Step {
    Some(Err(Failure {
        path: Some(path),
        message: bounded_error(message),
    }))
}
impl Discovery {
    pub fn begin(root: PathBuf, exclusions: Vec<String>) -> Result<Self, String> {
        validate_begin(&root, &exclusions)?;
        let live = root.canonicalize().map_err(|error| {
            format!(
                "canonicalize configured Match root at execution: {}",
                bounded_error(error)
            )
        })?;
        if live != root {
            return Err("configured Match root identity changed after opt-in".into());
        }
        Ok(Self {
            root,
            exclusions,
            stack: Vec::new(),
            pending: None,
            first: true,
            current: None,
        })
    }
    fn retained_paths(&self) -> usize {
        path_bytes(&self.root)
            + self
                .stack
                .iter()
                .map(|entry| path_bytes(&entry.path))
                .sum::<usize>()
    }
    fn observe(&mut self, path: PathBuf) -> Result<Step, String> {
        if path.as_os_str().len() > MAX_TEXT {
            return Err("Match discovery path bound exceeded".into());
        }
        let relative = path.strip_prefix(&self.root).map_err(bounded_error)?;
        if is_excluded(relative, &self.exclusions) {
            return Ok(None);
        }
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(value) => value,
            Err(error) => return Ok(failure(path, error)),
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if self.stack.len() >= MAX_DEPTH {
                return Ok(failure(path, "Match discovery depth exceeds 64 open directories; move or separately index the subtree"));
            }
            if self
                .retained_paths()
                .saturating_add(path_bytes(&path).saturating_mul(2))
                > MAX_STATE_PATH_BYTES
            {
                return Ok(failure(path, "Match discovery retained path budget exceeds 256 KiB; separately index the subtree"));
            }
            self.pending = Some(path.clone());
        }
        let is_file = metadata.file_type().is_file();
        self.current = Some(path.clone());
        Ok(Some(Ok(Entry { path, is_file })))
    }
    pub fn next(&mut self) -> Result<Step, String> {
        self.current = None;
        if self.first {
            self.first = false;
            if let Some(result) = self.observe(self.root.clone())? {
                return Ok(Some(result));
            }
        }
        loop {
            if let Some(path) = self.pending.take() {
                // Recheck before descent: a replaced symlink must never be followed.
                match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => (),
                    Ok(_) => {
                        return Ok(failure(
                            path,
                            "Match discovery directory changed before descent",
                        ))
                    }
                    Err(error) => return Ok(failure(path, error)),
                }
                match std::fs::read_dir(&path) {
                    Ok(entries) => self.stack.push(Directory { path, entries }),
                    Err(error) => return Ok(failure(path, error)),
                }
            }
            let Some(directory) = self.stack.last_mut() else {
                self.root = PathBuf::new();
                self.exclusions = Vec::new();
                self.stack = Vec::new();
                return Ok(None);
            };
            let next = match directory.entries.next() {
                None => {
                    self.stack.pop();
                    continue;
                }
                Some(Err(error)) => return Ok(failure(directory.path.clone(), error)),
                Some(Ok(entry)) => entry.path(),
            };
            if let Some(result) = self.observe(next)? {
                return Ok(Some(result));
            }
        }
    }
    pub fn metadata(&self, path: &Path) -> Result<u64, String> {
        if self.current.as_deref() != Some(path) {
            return Err("Match discovery metadata does not match the current entry".into());
        }
        std::fs::symlink_metadata(path)
            .map(|metadata| metadata.len())
            .map_err(bounded_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wp086_discovery_streams_wide_directories_and_reports_depth_overflow() {
        let path =
            std::env::temp_dir().join(format!("facial-discovery-bounds-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        for index in 0..257 {
            std::fs::write(path.join(format!("{index}.jpg")), b"x").unwrap();
        }
        let root = path.canonicalize().unwrap();
        let mut discovery = Discovery::begin(root.clone(), vec![]).unwrap();
        let mut files = 0;
        while let Some(entry) = discovery.next().unwrap() {
            if entry.unwrap().is_file {
                files += 1;
            }
            assert!(discovery.stack.len() <= 1);
            assert!(discovery.retained_paths() <= MAX_STATE_PATH_BYTES);
        }
        assert_eq!(files, 257);
        let mut deepest = root.clone();
        for _ in 0..MAX_DEPTH {
            deepest.push("d");
            std::fs::create_dir(&deepest).unwrap();
        }
        let mut discovery = Discovery::begin(root.clone(), vec![]).unwrap();
        let mut failures = Vec::new();
        while let Some(entry) = discovery.next().unwrap() {
            if let Err(error) = entry {
                failures.push(error);
            }
            assert!(discovery.stack.len() <= MAX_DEPTH);
        }
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path(), Some(deepest.as_path()));
        assert!(failures[0].to_string().contains("depth exceeds"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
