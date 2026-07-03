//! The per-lineage scratchpad — `scratch_read` / `scratch_write` /
//! `scratch_list` capabilities plus the copy-on-fork directory management
//! (ARCHITECTURE §19.3, ROADMAP T0.4).
//!
//! Each ask runs against a private scratch directory. The three capabilities
//! are **ungated** (`requires_permission = false`) — the scratch root is a
//! host-owned jail, so writing inside it needs no permission round-trip — and
//! every path is canonicalized and checked to stay under the root, mirroring
//! the `hugr-docs` read-only path discipline exactly (reject absolute paths and
//! any `..`/root/prefix component, then confirm the canonical path is still
//! under the root).
//!
//! ## Lifetime & copy-on-fork
//!
//! Scratch state follows the **trace lineage**, not the process. Each finalized
//! trace owns a subtree `…/<scratch_root>/<trace_id>`; a resumed ask **seeds**
//! its working directory by copying the parent's subtree, so it sees the
//! ancestor's notes. Because the copy is per-ask, two asks that fork the same
//! parent get independent working copies — a divergence-safe copy-on-fork:
//! sibling branches can never observe each other's writes (§19.3). The working
//! copy is finalized to its own `<trace_id>` subtree only after the ask's trace
//! is persisted (the id is content-derived, so it is not known until then).

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde_json::json;

/// Schemas for the built-in scratchpad tools. Used by `Agent::describe`
/// without needing to construct a per-ask scratch jail.
pub(crate) fn scratch_tool_schemas() -> Vec<ToolSchema> {
    vec![
        scratch_write_schema(),
        scratch_read_schema(),
        scratch_list_schema(),
    ]
}

/// The canonicalized scratch root one ask's tools are jailed to. Cheap to
/// clone (an `Arc`), so the same root backs all three capabilities.
#[derive(Clone)]
pub(crate) struct ScratchDir {
    root: Arc<PathBuf>,
}

impl ScratchDir {
    /// Wrap an already-created directory as a scratch jail, canonicalizing it so
    /// later `starts_with` checks compare canonical prefixes.
    pub(crate) fn new(root: impl AsRef<Path>) -> std::io::Result<Self> {
        let root = root.as_ref().canonicalize()?;
        Ok(Self {
            root: Arc::new(root),
        })
    }

    fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Resolve a relative path that must already exist inside the jail (reads,
    /// listing). Rejects absolute paths and any escaping component, then
    /// confirms the canonical target is still under the root.
    fn resolve_existing(&self, rel: &str) -> Result<PathBuf, String> {
        let candidate = self.resolve_rel(rel)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("path does not exist inside scratch root: {rel}: {e}"))?;
        if !canonical.starts_with(self.root()) {
            return Err(format!("path escapes scratch root: {rel}"));
        }
        Ok(canonical)
    }

    /// Resolve a relative path for writing (the file need not exist yet). The
    /// parent directory is created and canonicalized, and the canonical parent
    /// must stay under the root — so a symlinked parent can't escape the jail.
    fn resolve_for_write(&self, rel: &str) -> Result<PathBuf, String> {
        let candidate = self.resolve_rel(rel)?;
        if candidate == *self.root() {
            return Err("path must name a file, not the scratch root".to_string());
        }
        let file_name = candidate
            .file_name()
            .ok_or_else(|| format!("path must name a file: {rel}"))?
            .to_owned();
        let parent = candidate
            .parent()
            .ok_or_else(|| format!("path must name a file: {rel}"))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create scratch directory for {rel}: {e}"))?;
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| format!("failed to resolve scratch directory for {rel}: {e}"))?;
        if !canonical_parent.starts_with(self.root()) {
            return Err(format!("path escapes scratch root: {rel}"));
        }
        Ok(canonical_parent.join(file_name))
    }

    /// The shared jail check: reject absolute paths and any `..`/root/prefix
    /// component (same discipline as `hugr-docs`), then join under the root.
    fn resolve_rel(&self, rel: &str) -> Result<PathBuf, String> {
        let rel = rel.trim();
        if rel.is_empty() {
            return Ok(self.root().to_path_buf());
        }
        let path = Path::new(rel);
        if path.is_absolute() {
            return Err("path must be relative to scratch root".to_string());
        }
        for component in path.components() {
            match component {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("path escapes scratch root: {rel}"));
                }
            }
        }
        Ok(self.root().join(path))
    }

    fn rel_path(&self, path: &Path) -> String {
        let rel = path.strip_prefix(self.root()).unwrap_or(path);
        rel.components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// The three scratchpad capabilities rooted at this jail, ready to register
    /// on an ask's engine.
    pub(crate) fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(ScratchWrite { dir: self.clone() }),
            Arc::new(ScratchRead { dir: self.clone() }),
            Arc::new(ScratchList { dir: self.clone() }),
        ]
    }
}

/// Recursively copy `from` into `to` (used to seed a fork/resume from its
/// parent's finalized subtree). `to` is created if absent; only regular files
/// and directories are copied (scratchpads are small by construction, §19.3).
pub(crate) fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&src, &dst)?;
        } else if file_type.is_file() {
            fs::copy(&src, &dst)?;
        }
        // Symlinks and other exotic entries are skipped — the jail deals in
        // plain files/dirs only.
    }
    Ok(())
}

/// Write text to a file inside the scratch root. Ungated (§19.3): the jail is
/// the boundary, so no permission round-trip.
struct ScratchWrite {
    dir: ScratchDir,
}

#[async_trait]
impl Capability for ScratchWrite {
    fn name(&self) -> &str {
        "scratch_write"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        scratch_write_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let rel = match args.get("path").and_then(Value::as_str) {
            Some(rel) => rel,
            None => return Err(json!({ "error": "scratch_write requires string `path`" })),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(content) => content,
            None => return Err(json!({ "error": "scratch_write requires string `content`" })),
        };
        let path = match self.dir.resolve_for_write(rel) {
            Ok(path) => path,
            Err(error) => return Err(json!({ "error": error })),
        };
        match fs::write(&path, content) {
            Ok(()) => Ok(json!({
                "path": self.dir.rel_path(&path),
                "bytes_written": content.len(),
            })),
            Err(e) => Err(json!({ "error": format!("failed to write {rel}: {e}") })),
        }
    }
}

/// Read a UTF-8 text file from inside the scratch root. Ungated (read-only).
struct ScratchRead {
    dir: ScratchDir,
}

#[async_trait]
impl Capability for ScratchRead {
    fn name(&self) -> &str {
        "scratch_read"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        scratch_read_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let rel = match args.get("path").and_then(Value::as_str) {
            Some(rel) => rel,
            None => return Err(json!({ "error": "scratch_read requires string `path`" })),
        };
        let path = match self.dir.resolve_existing(rel) {
            Ok(path) => path,
            Err(error) => return Err(json!({ "error": error })),
        };
        if !path.is_file() {
            return Err(json!({ "error": format!("scratch_read path is not a file: {rel}") }));
        }
        match fs::read_to_string(&path) {
            Ok(content) => Ok(json!({
                "path": self.dir.rel_path(&path),
                "content": content,
            })),
            Err(e) => Err(json!({ "error": format!("failed to read {rel}: {e}") })),
        }
    }
}

/// List entries under a directory of the scratch root. Ungated (read-only).
struct ScratchList {
    dir: ScratchDir,
}

#[async_trait]
impl Capability for ScratchList {
    fn name(&self) -> &str {
        "scratch_list"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        scratch_list_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let rel = args.get("path").and_then(Value::as_str).unwrap_or("");
        let start = match self.dir.resolve_existing(rel) {
            Ok(path) => path,
            Err(error) => return Err(json!({ "error": error })),
        };
        if !start.is_dir() {
            return Err(json!({ "error": format!("scratch_list path is not a directory: {rel}") }));
        }
        let mut read_dir = match fs::read_dir(&start) {
            Ok(entries) => entries.collect::<Result<Vec<_>, _>>(),
            Err(e) => return Err(json!({ "error": format!("failed to list {rel}: {e}") })),
        }
        .map_err(|e| json!({ "error": format!("failed to list {rel}: {e}") }))?;
        // Deterministic order regardless of directory-entry order.
        read_dir.sort_by_key(|entry| entry.file_name());
        let mut entries = Vec::new();
        for entry in read_dir {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            entries.push(json!({
                "path": self.dir.rel_path(&path),
                "kind": if metadata.is_dir() { "dir" } else { "file" },
                "bytes": if metadata.is_file() { Some(metadata.len()) } else { None },
            }));
        }
        Ok(json!({ "entries": entries }))
    }
}

fn scratch_write_schema() -> ToolSchema {
    ToolSchema::new(
        "scratch_write",
        "Write text to a file in your private scratch directory, creating or overwriting it. Paths are relative to the scratch root; parent directories are created as needed.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the scratch root." },
                "content": { "type": "string", "description": "The full contents to write." }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    )
}

fn scratch_read_schema() -> ToolSchema {
    ToolSchema::new(
        "scratch_read",
        "Read a UTF-8 text file from your private scratch directory. Paths are relative to the scratch root.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path relative to the scratch root." }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    )
}

fn scratch_list_schema() -> ToolSchema {
    ToolSchema::new(
        "scratch_list",
        "List files and directories in your private scratch directory. Paths are relative to the scratch root; the default is the root itself.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Relative directory path. Defaults to the scratch root." }
            },
            "additionalProperties": false
        }),
    )
}
