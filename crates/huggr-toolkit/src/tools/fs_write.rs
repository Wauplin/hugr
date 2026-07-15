use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

/// A single canonicalized write jail: the path-resolution primitives, each
/// re-checking against this jail's root after canonicalization.
struct WriteJail {
    root: PathBuf,
}

impl WriteJail {
    fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing fs_write root {}", root.as_ref().display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "fs_write root is not a directory: {}",
            root.display()
        );
        Ok(Self { root })
    }

    fn relative(&self, rel: &str) -> Result<PathBuf> {
        let path = Path::new(rel);
        anyhow::ensure!(
            !path.is_absolute(),
            "path must be relative to the tool root"
        );
        anyhow::ensure!(
            path.components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
            "path escapes the tool root"
        );
        Ok(self.root.join(path))
    }

    fn resolve_parent(&self, rel: &str) -> Result<PathBuf> {
        let candidate = self.relative(rel)?;
        let parent = candidate
            .parent()
            .context("path has no parent")?
            .canonicalize()
            .with_context(|| format!("parent directory does not exist for {rel}"))?;
        anyhow::ensure!(parent.starts_with(&self.root), "path escapes the tool root");
        Ok(parent.join(candidate.file_name().context("path has no file name")?))
    }

    fn resolve_existing(&self, rel: &str) -> Result<PathBuf> {
        let path = self
            .relative(rel)?
            .canonicalize()
            .with_context(|| format!("path does not exist inside the tool root: {rel}"))?;
        anyhow::ensure!(path.starts_with(&self.root), "path escapes the tool root");
        Ok(path)
    }

    fn resolve_write(&self, rel: &str) -> Result<PathBuf> {
        let candidate = self.relative(rel)?;
        if candidate.symlink_metadata().is_ok() {
            return self.resolve_existing(rel);
        }
        self.resolve_parent(rel)
    }
}

struct NamedWriteJail {
    name: String,
    jail: WriteJail,
}

#[derive(Clone)]
/// One or more named canonicalized write jails shared by the write
/// capabilities. Callers address files as `<root-name>/<path>`.
pub struct FsWriteRoot {
    jails: Arc<Vec<NamedWriteJail>>,
}

impl FsWriteRoot {
    /// Build from named roots. Every root is addressed as `<name>/<path>`;
    /// names must be unique and free of `/`.
    pub fn with_named(roots: Vec<(String, PathBuf)>) -> Result<Self> {
        anyhow::ensure!(!roots.is_empty(), "fs_write requires at least one root");
        let mut jails = Vec::with_capacity(roots.len());
        let mut seen = std::collections::HashSet::new();
        for (name, path) in roots {
            anyhow::ensure!(!name.is_empty(), "root name cannot be empty");
            anyhow::ensure!(!name.contains('/'), "root name `{name}` cannot contain `/`");
            anyhow::ensure!(seen.insert(name.clone()), "duplicate root name `{name}`");
            let jail = WriteJail::new(&path)?;
            jails.push(NamedWriteJail { name, jail });
        }
        Ok(Self {
            jails: Arc::new(jails),
        })
    }

    /// Build the write, edit, directory-creation, and removal capabilities.
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(FsWrite(self.clone())),
            Arc::new(FsEdit(self.clone())),
            Arc::new(FsCreateDir(self.clone())),
            Arc::new(FsRemove(self.clone())),
        ]
    }

    fn root_names(&self) -> Vec<&str> {
        self.jails.iter().map(|j| j.name.as_str()).collect()
    }

    /// Split a caller path into (jail, jail-relative sub-path). The first
    /// segment always names a root; the sub-path is empty only when the caller
    /// named a root with no file under it.
    fn locate(&self, rel: &str) -> Result<(&WriteJail, String)> {
        let rel = rel.trim().trim_start_matches('/');
        let (name, sub) = match rel.split_once('/') {
            Some((name, sub)) => (name, sub),
            None => (rel, ""),
        };
        let jail = self
            .jails
            .iter()
            .find(|j| j.name == name)
            .with_context(|| {
                format!(
                    "unknown root `{name}`; known roots: {}",
                    self.root_names().join(", ")
                )
            })?;
        Ok((&jail.jail, sub.trim().to_string()))
    }

    fn resolve_write(&self, rel: &str) -> Result<PathBuf> {
        let (jail, sub) = self.locate(rel)?;
        anyhow::ensure!(!sub.is_empty(), "a file path is required, not just a root");
        jail.resolve_write(&sub)
    }

    fn resolve_parent(&self, rel: &str) -> Result<PathBuf> {
        let (jail, sub) = self.locate(rel)?;
        anyhow::ensure!(!sub.is_empty(), "a path is required, not just a root");
        jail.resolve_parent(&sub)
    }

    fn resolve_existing(&self, rel: &str) -> Result<PathBuf> {
        let (jail, sub) = self.locate(rel)?;
        anyhow::ensure!(!sub.is_empty(), "a file path is required, not just a root");
        jail.resolve_existing(&sub)
    }

    /// Whether a canonical path is exactly one of the jail roots.
    fn is_jail_root(&self, path: &Path) -> bool {
        self.jails.iter().any(|j| j.jail.root == path)
    }
}

struct FsWrite(FsWriteRoot);
struct FsEdit(FsWriteRoot);
struct FsCreateDir(FsWriteRoot);
struct FsRemove(FsWriteRoot);

fn wrap(result: Result<Value>) -> std::result::Result<Value, Value> {
    result.map_err(|e| json!({"error":e.to_string()}))
}

#[async_trait]
impl Capability for FsWrite {
    fn name(&self) -> &str {
        "fs_write"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_write",
            "Create or replace one file, addressed as `<root>/<path>`. Parent directories must already exist.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"File path as `<root>/<path>`."},"content":{"type":"string"},"append":{"type":"boolean","description":"Append instead of replacing. Defaults to false."}},"required":["path","content"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_write requires string `path`")?;
            let content = args
                .get("content")
                .and_then(Value::as_str)
                .context("fs_write requires string `content`")?;
            let path = self.0.resolve_write(rel)?;
            if args.get("append").and_then(Value::as_bool).unwrap_or(false) {
                use std::io::Write;
                let mut file = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)?;
                file.write_all(content.as_bytes())?;
            } else {
                fs::write(&path, content)?;
            }
            Ok(json!({"path":rel,"bytes_written":content.len()}))
        })())
    }
}

#[async_trait]
impl Capability for FsEdit {
    fn name(&self) -> &str {
        "fs_edit"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_edit",
            "Replace an exact text occurrence in one existing file, addressed as `<root>/<path>`. `old` must match verbatim and, unless `replace_all` is set, must be unique.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"File path as `<root>/<path>`."},"old":{"type":"string","description":"Exact text to find. Must be non-empty."},"new":{"type":"string","description":"Replacement text."},"replace_all":{"type":"boolean","description":"Replace every occurrence instead of requiring a unique match. Defaults to false."}},"required":["path","old","new"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_edit requires string `path`")?;
            let old = args
                .get("old")
                .and_then(Value::as_str)
                .context("fs_edit requires string `old`")?;
            let new = args
                .get("new")
                .and_then(Value::as_str)
                .context("fs_edit requires string `new`")?;
            anyhow::ensure!(!old.is_empty(), "`old` must be non-empty");
            anyhow::ensure!(old != new, "`old` and `new` are identical; nothing to edit");
            let replace_all = args
                .get("replace_all")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let path = self.0.resolve_existing(rel)?;
            let content = fs::read_to_string(&path)
                .with_context(|| format!("reading {rel} for edit (must be UTF-8 text)"))?;
            let count = content.matches(old).count();
            anyhow::ensure!(count > 0, "`old` text not found in {rel}");
            anyhow::ensure!(
                replace_all || count == 1,
                "`old` occurs {count} times in {rel}; pass a longer unique match or set replace_all"
            );
            let updated = if replace_all {
                content.replace(old, new)
            } else {
                content.replacen(old, new, 1)
            };
            fs::write(&path, &updated)?;
            Ok(json!({"path":rel,"replacements":if replace_all {count} else {1}}))
        })())
    }
}

#[async_trait]
impl Capability for FsCreateDir {
    fn name(&self) -> &str {
        "fs_create_dir"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_create_dir",
            "Create one directory, addressed as `<root>/<path>`. Its parent must already exist.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"Directory as `<root>/<path>`."}},"required":["path"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_create_dir requires string `path`")?;
            fs::create_dir(self.0.resolve_parent(rel)?)?;
            Ok(json!({"path":rel}))
        })())
    }
}

#[async_trait]
impl Capability for FsRemove {
    fn name(&self) -> &str {
        "fs_remove"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_remove",
            "Remove one file or empty directory, addressed as `<root>/<path>`. Recursive removal is unavailable.",
            json!({"type":"object","properties":{"path":{"type":"string","description":"File or empty directory as `<root>/<path>`."}},"required":["path"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_remove requires string `path`")?;
            anyhow::ensure!(!rel.trim().is_empty(), "cannot remove the tool root");
            let path = self.0.resolve_existing(rel)?;
            // `resolve_existing` canonicalizes, so `.`/`a/..`-style paths that
            // name a jail itself compare equal to that root here.
            anyhow::ensure!(!self.0.is_jail_root(&path), "cannot remove the tool root");
            if path.is_dir() {
                fs::remove_dir(path)?;
            } else {
                fs::remove_file(path)?;
            }
            Ok(json!({"path":rel}))
        })())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn resolves_writes_inside_root_and_rejects_escape() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let root = FsWriteRoot::with_named(vec![("r".to_string(), dir.clone())]).unwrap();
        fs::write(root.resolve_parent("r/x.txt").unwrap(), "ok").unwrap();
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "ok");
        assert!(root.resolve_parent("r/../x").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fs_remove_refuses_to_delete_the_jail_root() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-remove-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let remove =
            FsRemove(FsWriteRoot::with_named(vec![("r".to_string(), dir.clone())]).unwrap());
        let sink = ChunkSink::noop();
        // Naming the root with no file, resolving back to the root via `.`, and
        // escaping via `..` must all be refused without touching the jail.
        for path in ["r", "r/.", "r/sub/.."] {
            let err = remove
                .invoke(json!({ "path": path }), &sink)
                .await
                .unwrap_err();
            assert!(
                err["error"].as_str().unwrap().contains("root")
                    || err["error"].as_str().unwrap().contains("exist"),
                "{path}: {err}"
            );
            assert!(dir.is_dir(), "jail root survived `{path}`");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fs_edit_replaces_a_unique_occurrence() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-edit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.txt"), "hello world").unwrap();
        let edit = FsEdit(FsWriteRoot::with_named(vec![("r".to_string(), dir.clone())]).unwrap());
        let out = edit
            .invoke(
                json!({ "path": "r/a.txt", "old": "world", "new": "there" }),
                &ChunkSink::noop(),
            )
            .await
            .unwrap();
        assert_eq!(out["replacements"], 1);
        assert_eq!(
            fs::read_to_string(dir.join("a.txt")).unwrap(),
            "hello there"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fs_edit_rejects_ambiguous_match_but_replace_all_takes_it() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-edit-amb-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.txt"), "x x x").unwrap();
        let edit = FsEdit(FsWriteRoot::with_named(vec![("r".to_string(), dir.clone())]).unwrap());
        let sink = ChunkSink::noop();
        let err = edit
            .invoke(json!({ "path": "r/a.txt", "old": "x", "new": "y" }), &sink)
            .await
            .unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("3 times"));
        let out = edit
            .invoke(
                json!({ "path": "r/a.txt", "old": "x", "new": "y", "replace_all": true }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(out["replacements"], 3);
        assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "y y y");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fs_edit_errors_on_missing_text_and_missing_file() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-edit-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("a.txt"), "abc").unwrap();
        let edit = FsEdit(FsWriteRoot::with_named(vec![("r".to_string(), dir.clone())]).unwrap());
        let sink = ChunkSink::noop();
        let missing_text = edit
            .invoke(
                json!({ "path": "r/a.txt", "old": "zzz", "new": "q" }),
                &sink,
            )
            .await
            .unwrap_err();
        assert!(
            missing_text["error"]
                .as_str()
                .unwrap()
                .contains("not found")
        );
        let missing_file = edit
            .invoke(
                json!({ "path": "r/nope.txt", "old": "a", "new": "b" }),
                &sink,
            )
            .await
            .unwrap_err();
        assert!(
            missing_file["error"]
                .as_str()
                .unwrap()
                .contains("does not exist")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn multi_root_writes_edits_and_protects_each_root() {
        let base = std::env::temp_dir().join(format!("huggr-fsw-multi-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        let root = FsWriteRoot::with_named(vec![
            ("a".to_string(), a.clone()),
            ("b".to_string(), b.clone()),
        ])
        .unwrap();
        let sink = ChunkSink::noop();

        // Write and edit addressed by `<root>/<path>`.
        FsWrite(root.clone())
            .invoke(json!({ "path": "b/notes.txt", "content": "hello" }), &sink)
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(b.join("notes.txt")).unwrap(), "hello");
        FsEdit(root.clone())
            .invoke(
                json!({ "path": "b/notes.txt", "old": "hello", "new": "bye" }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(fs::read_to_string(b.join("notes.txt")).unwrap(), "bye");
        // The other root is untouched and has no such file.
        assert!(!a.join("notes.txt").exists());

        // Unknown root, jail-root removal, and cross-root traversal all fail.
        let unknown = FsWrite(root.clone())
            .invoke(json!({ "path": "c/x.txt", "content": "x" }), &sink)
            .await
            .unwrap_err();
        assert!(unknown["error"].as_str().unwrap().contains("unknown root"));
        let remove_root = FsRemove(root.clone())
            .invoke(json!({ "path": "a" }), &sink)
            .await
            .unwrap_err();
        assert!(remove_root["error"].as_str().unwrap().contains("root"));
        let traversal = FsWrite(root.clone())
            .invoke(json!({ "path": "a/../b/evil.txt", "content": "x" }), &sink)
            .await
            .unwrap_err();
        assert!(traversal["error"].as_str().unwrap().contains("escapes"));
        assert!(!b.join("evil.txt").exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_existing_symlink_write_that_escapes_root() {
        use std::os::unix::fs::symlink;
        let base = std::env::temp_dir().join(format!("huggr-fs-write-link-{}", std::process::id()));
        let root_dir = base.join("root");
        let outside = base.join("outside.txt");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&root_dir).unwrap();
        fs::write(&outside, "safe").unwrap();
        symlink(&outside, root_dir.join("link")).unwrap();
        let root = FsWriteRoot::with_named(vec![("r".to_string(), root_dir.clone())]).unwrap();
        assert!(root.resolve_write("r/link").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
        fs::remove_dir_all(base).unwrap();
    }
}
