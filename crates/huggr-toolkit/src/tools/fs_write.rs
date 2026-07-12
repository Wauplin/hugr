use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

#[derive(Clone)]
/// A canonicalized writable root shared by the filesystem write capabilities.
pub struct FsWriteRoot {
    root: Arc<PathBuf>,
}

impl FsWriteRoot {
    /// Canonicalize a directory used as the write jail.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing fs_write root {}", root.as_ref().display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "fs_write root is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// Build the write, directory-creation, and removal capabilities.
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(FsWrite(self.clone())),
            Arc::new(FsCreateDir(self.clone())),
            Arc::new(FsRemove(self.clone())),
        ]
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
        anyhow::ensure!(
            parent.starts_with(self.root.as_path()),
            "path escapes the tool root"
        );
        Ok(parent.join(candidate.file_name().context("path has no file name")?))
    }

    fn resolve_existing(&self, rel: &str) -> Result<PathBuf> {
        let path = self
            .relative(rel)?
            .canonicalize()
            .with_context(|| format!("path does not exist inside the tool root: {rel}"))?;
        anyhow::ensure!(
            path.starts_with(self.root.as_path()),
            "path escapes the tool root"
        );
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

struct FsWrite(FsWriteRoot);
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
            "Create or replace one file under the configured root. Parent directories must already exist.",
            json!({"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"},"append":{"type":"boolean","description":"Append instead of replacing. Defaults to false."}},"required":["path","content"],"additionalProperties":false}),
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
impl Capability for FsCreateDir {
    fn name(&self) -> &str {
        "fs_create_dir"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_create_dir",
            "Create one directory under the configured root. Its parent must already exist.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
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
            "Remove one file or empty directory under the configured root. Recursive removal is unavailable.",
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
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
            // name the jail itself compare equal to the root here.
            anyhow::ensure!(path != *self.0.root, "cannot remove the tool root");
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
        let root = FsWriteRoot::new(&dir).unwrap();
        fs::write(root.resolve_parent("x.txt").unwrap(), "ok").unwrap();
        assert_eq!(fs::read_to_string(dir.join("x.txt")).unwrap(), "ok");
        assert!(root.resolve_parent("../x").is_err());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fs_remove_refuses_to_delete_the_jail_root() {
        let dir = std::env::temp_dir().join(format!("huggr-fs-remove-root-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir(&dir).unwrap();
        let remove = FsRemove(FsWriteRoot::new(&dir).unwrap());
        let sink = ChunkSink::noop();
        for path in [".", "./", "sub/.."] {
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
        let root = FsWriteRoot::new(&root_dir).unwrap();
        assert!(root.resolve_write("link").is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "safe");
        fs::remove_dir_all(base).unwrap();
    }
}
