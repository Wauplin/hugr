//! Agent-wide durable memory tools.

use std::fs::{self, OpenOptions};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

#[derive(Clone, Debug)]
pub struct FsMemory {
    root: Arc<PathBuf>,
    readonly: bool,
    write_lock: Arc<Mutex<()>>,
}

impl FsMemory {
    pub fn new(root: impl Into<PathBuf>, readonly: bool) -> Self {
        Self {
            root: Arc::new(root.into()),
            readonly,
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(MemoryWrite(self.clone())),
            Arc::new(MemoryRead(self.clone())),
            Arc::new(MemoryList(self.clone())),
        ]
    }

    fn resolve_existing(&self, rel: &str) -> Result<PathBuf, String> {
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("memory root is not available: {e}"))?;
        let candidate = resolve_rel(&root, rel)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|e| format!("path does not exist inside memory root: {rel}: {e}"))?;
        if !canonical.starts_with(&root) {
            return Err(format!("path escapes memory root: {rel}"));
        }
        Ok(canonical)
    }

    fn resolve_for_write(&self, rel: &str) -> Result<PathBuf, String> {
        fs::create_dir_all(self.root.as_path())
            .map_err(|e| format!("failed to create memory root: {e}"))?;
        let root = self
            .root
            .canonicalize()
            .map_err(|e| format!("memory root is not available: {e}"))?;
        let candidate = resolve_rel(&root, rel)?;
        if candidate == root {
            return Err("path must name a file, not the memory root".to_string());
        }
        let file_name = candidate
            .file_name()
            .ok_or_else(|| format!("path must name a file: {rel}"))?
            .to_owned();
        let parent = candidate
            .parent()
            .ok_or_else(|| format!("path must name a file: {rel}"))?;
        fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create memory directory for {rel}: {e}"))?;
        let canonical_parent = parent
            .canonicalize()
            .map_err(|e| format!("failed to resolve memory directory for {rel}: {e}"))?;
        if !canonical_parent.starts_with(&root) {
            return Err(format!("path escapes memory root: {rel}"));
        }
        Ok(canonical_parent.join(file_name))
    }

    fn lock_file(&self) -> PathBuf {
        self.root.join(".memory-write.lock")
    }

    fn acquire_file_lock(&self) -> Option<PathBuf> {
        let path = self.lock_file();
        for _ in 0..100 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Some(path),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return None,
            }
        }
        None
    }
}

struct MemoryWrite(FsMemory);

#[async_trait]
impl Capability for MemoryWrite {
    fn name(&self) -> &str {
        "memory_write"
    }

    fn schema(&self) -> ToolSchema {
        memory_write_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        if self.0.readonly {
            return Err(json!({ "error": "memory is readonly" }));
        }
        let rel = match args.get("path").and_then(Value::as_str) {
            Some(rel) => rel,
            None => return Err(json!({ "error": "memory_write requires string `path`" })),
        };
        let content = match args.get("content").and_then(Value::as_str) {
            Some(content) => content,
            None => return Err(json!({ "error": "memory_write requires string `content`" })),
        };
        let _guard = self.0.write_lock.lock().unwrap();
        let file_lock = self.0.acquire_file_lock();
        let result = (|| {
            let path = self.0.resolve_for_write(rel)?;
            fs::write(&path, content.as_bytes())
                .map_err(|e| format!("failed to write memory file {rel}: {e}"))?;
            let root = self
                .0
                .root
                .canonicalize()
                .map_err(|e| format!("memory root is not available: {e}"))?;
            Ok::<_, String>(rel_path_from(&root, &path))
        })();
        if let Some(path) = file_lock {
            let _ = fs::remove_file(path);
        }
        match result {
            Ok(path) => Ok(json!({ "path": path, "bytes_written": content.len() })),
            Err(error) => Err(json!({ "error": error })),
        }
    }
}

struct MemoryRead(FsMemory);

#[async_trait]
impl Capability for MemoryRead {
    fn name(&self) -> &str {
        "memory_read"
    }

    fn schema(&self) -> ToolSchema {
        memory_read_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let rel = match args.get("path").and_then(Value::as_str) {
            Some(rel) => rel,
            None => return Err(json!({ "error": "memory_read requires string `path`" })),
        };
        match self.0.resolve_existing(rel).and_then(|path| {
            if !path.is_file() {
                return Err(format!("memory_read path is not a file: {rel}"));
            }
            fs::read(&path).map_err(|e| format!("failed to read memory file {rel}: {e}"))
        }) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => Ok(json!({ "path": rel, "content": content })),
                Err(err) => {
                    Err(json!({ "error": format!("memory_read path is not UTF-8: {err}") }))
                }
            },
            Err(error) => Err(json!({ "error": error })),
        }
    }
}

struct MemoryList(FsMemory);

#[async_trait]
impl Capability for MemoryList {
    fn name(&self) -> &str {
        "memory_list"
    }

    fn schema(&self) -> ToolSchema {
        memory_list_schema()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let rel = args.get("path").and_then(Value::as_str).unwrap_or("");
        if let Err(err) = fs::create_dir_all(self.0.root.as_path()) {
            return Err(json!({ "error": format!("failed to create memory root: {err}") }));
        }
        let start = match self.0.resolve_existing(rel) {
            Ok(path) => path,
            Err(error) => return Err(json!({ "error": error })),
        };
        if !start.is_dir() {
            return Err(json!({ "error": format!("memory_list path is not a directory: {rel}") }));
        }
        let root = match self.0.root.canonicalize() {
            Ok(root) => root,
            Err(err) => {
                return Err(json!({ "error": format!("memory root is not available: {err}") }));
            }
        };
        let mut read_dir = match fs::read_dir(&start)
            .and_then(|dir| dir.collect::<Result<Vec<_>, _>>())
        {
            Ok(entries) => entries,
            Err(err) => {
                return Err(json!({ "error": format!("failed to list memory path {rel}: {err}") }));
            }
        };
        read_dir.sort_by_key(|entry| entry.file_name());
        let entries = read_dir
            .into_iter()
            .filter_map(|entry| {
                let path = entry.path();
                let metadata = entry.metadata().ok()?;
                Some(json!({
                    "path": rel_path_from(&root, &path),
                    "kind": if metadata.is_dir() { "dir" } else { "file" },
                    "bytes": metadata.is_file().then_some(metadata.len()),
                }))
            })
            .collect::<Vec<_>>();
        Ok(json!({ "path": rel, "entries": entries }))
    }
}

fn resolve_rel(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err("path must be relative to memory root".to_string());
    }
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path escapes memory root: {rel}"));
            }
        }
    }
    Ok(root.join(path))
}

fn rel_path_from(root: &Path, path: &Path) -> String {
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

pub fn memory_tool_schemas() -> Vec<ToolSchema> {
    vec![
        memory_write_schema(),
        memory_read_schema(),
        memory_list_schema(),
    ]
}

fn memory_write_schema() -> ToolSchema {
    ToolSchema::new(
        "memory_write",
        "Write text to durable agent-wide memory. Paths are relative to the memory root.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        }),
    )
}

fn memory_read_schema() -> ToolSchema {
    ToolSchema::new(
        "memory_read",
        "Read a UTF-8 text file from durable agent-wide memory.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    )
}

fn memory_list_schema() -> ToolSchema {
    ToolSchema::new(
        "memory_list",
        "List files and directories in durable agent-wide memory.",
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "additionalProperties": false
        }),
    )
}
