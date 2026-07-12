//! Test-only helpers shared by this crate's unit and integration tests.
//!
//! `#[doc(hidden)]` and **not** part of the public API: it must be an ordinary
//! `pub` module (not `#[cfg(test)]`) only so the `tests/` integration binaries
//! can reach it. Do not use it outside Huggr's own tests.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// An RAII temporary directory for tests: a unique path under the system temp
/// root (pid + creation nanos make it collision-free across parallel test
/// processes), created eagerly and removed on `Drop` — so a failing test
/// doesn't leak directories.
pub struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `…/huggr-<tag>-<pid>-<nanos>` and return the guard.
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("huggr-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// The directory's path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
