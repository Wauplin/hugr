//! Filesystem-jail primitives shared by the scratchpad and memory backends.
//!
//! Both are trusted host storage backends jailed to a root directory. Keeping
//! the traversal and escape checks here means a hardening fix applies to every
//! jailed root at once instead of one copy silently diverging. `label` names
//! the root in error messages (for example "scratch" or "memory").

use std::path::{Component, Path, PathBuf};

/// Join `rel` under `root`, rejecting absolute paths and any `..`/root/prefix
/// component. An empty `rel` resolves to the root itself.
pub(crate) fn resolve_rel(root: &Path, rel: &str, label: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err(format!("path must be relative to {label} root"));
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path escapes {label} root: {rel}"));
            }
        }
    }
    Ok(root.join(path))
}

/// Render `path` relative to `root` as a `/`-joined string of its normal
/// components.
pub(crate) fn rel_path_from(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Resolve an existing file under an already-canonicalized `root`, verifying
/// the canonical target stays inside the jail.
pub(crate) fn resolve_existing(root: &Path, rel: &str, label: &str) -> Result<PathBuf, String> {
    let candidate = resolve_rel(root, rel, label)?;
    let canonical = candidate
        .canonicalize()
        .map_err(|e| format!("path does not exist inside {label} root: {rel}: {e}"))?;
    if !canonical.starts_with(root) {
        return Err(format!("path escapes {label} root: {rel}"));
    }
    Ok(canonical)
}

/// Resolve a writable file path under an already-canonicalized `root`, creating
/// parent directories and verifying the canonical parent stays inside the jail.
pub(crate) fn resolve_for_write(root: &Path, rel: &str, label: &str) -> Result<PathBuf, String> {
    let candidate = resolve_rel(root, rel, label)?;
    if candidate == *root {
        return Err(format!("path must name a file, not the {label} root"));
    }
    let file_name = candidate
        .file_name()
        .ok_or_else(|| format!("path must name a file: {rel}"))?
        .to_owned();
    let parent = candidate
        .parent()
        .ok_or_else(|| format!("path must name a file: {rel}"))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create {label} directory for {rel}: {e}"))?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("failed to resolve {label} directory for {rel}: {e}"))?;
    if !canonical_parent.starts_with(root) {
        return Err(format!("path escapes {label} root: {rel}"));
    }
    Ok(canonical_parent.join(file_name))
}
