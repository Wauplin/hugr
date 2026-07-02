//! Test-only helpers shared by the host's unit tests (`#[cfg(test)]`-gated in
//! `lib.rs`; never compiled into the library proper).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// An RAII temporary directory for tests: a unique path under the system temp
/// root (pid + creation nanos make it collision-free across parallel test
/// processes), created eagerly and removed on `Drop` — so a failing test
/// doesn't leak directories.
pub(crate) struct TempDir {
    path: PathBuf,
}

impl TempDir {
    /// Create `…/hugr-<tag>-<pid>-<nanos>` and return the guard.
    pub(crate) fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("hugr-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    /// The directory's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
