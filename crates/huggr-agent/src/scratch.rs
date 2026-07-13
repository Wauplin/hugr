//! The per-lineage scratchpad: `scratch_read` / `scratch_write` /
//! `scratch_list` capabilities plus copy-on-fork state management.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

use crate::TraceId;

const PENDING_DIRNAME: &str = ".pending";

/// Opaque scratch working-copy id.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScratchHandle(String);

impl ScratchHandle {
    fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScratchEntry {
    pub path: String,
    pub kind: ScratchEntryKind,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScratchEntryKind {
    File,
    Dir,
}

#[async_trait]
pub trait ScratchBackend: Send + Sync {
    async fn prepare(&self, parent: Option<&TraceId>) -> Result<ScratchHandle, std::io::Error>;
    async fn finalize(
        &self,
        handle: &ScratchHandle,
        trace_id: &TraceId,
    ) -> Result<(), std::io::Error>;
    async fn write_bytes(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<String, String>;
    async fn read_bytes(&self, handle: &ScratchHandle, rel_path: &str) -> Result<Vec<u8>, String>;
    async fn import_file(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
        source: &Path,
    ) -> Result<String, String> {
        let bytes = fs::read(source)
            .map_err(|e| format!("failed to read imported file {rel_path}: {e}"))?;
        self.write_bytes(handle, rel_path, &bytes).await
    }
    async fn local_path(&self, _handle: &ScratchHandle, _rel_path: &str) -> Option<PathBuf> {
        None
    }
    async fn list(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
    ) -> Result<Vec<ScratchEntry>, String>;
}

#[derive(Clone)]
pub(crate) struct ScratchSession {
    backend: Arc<dyn ScratchBackend>,
    handle: ScratchHandle,
}

impl ScratchSession {
    pub(crate) fn new(backend: Arc<dyn ScratchBackend>, handle: ScratchHandle) -> Self {
        Self { backend, handle }
    }

    pub(crate) fn handle(&self) -> &ScratchHandle {
        &self.handle
    }

    pub(crate) async fn write_bytes(&self, rel_path: &str, bytes: &[u8]) -> Result<String, String> {
        self.backend
            .write_bytes(&self.handle, rel_path, bytes)
            .await
    }

    pub(crate) async fn read_bytes(&self, rel_path: &str) -> Result<Vec<u8>, String> {
        self.backend.read_bytes(&self.handle, rel_path).await
    }

    pub(crate) async fn import_file(
        &self,
        rel_path: &str,
        source: &Path,
    ) -> Result<String, String> {
        self.backend
            .import_file(&self.handle, rel_path, source)
            .await
    }

    pub(crate) async fn local_path(&self, rel_path: &str) -> Option<PathBuf> {
        self.backend.local_path(&self.handle, rel_path).await
    }

    pub(crate) async fn list(&self, rel_path: &str) -> Result<Vec<ScratchEntry>, String> {
        self.backend.list(&self.handle, rel_path).await
    }

    pub(crate) fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(ScratchWrite {
                session: self.clone(),
            }),
            Arc::new(ScratchRead {
                session: self.clone(),
            }),
            Arc::new(ScratchList {
                session: self.clone(),
            }),
        ]
    }
}

#[derive(Debug)]
pub struct FsScratch {
    root: PathBuf,
    next: AtomicU64,
}

impl FsScratch {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            next: AtomicU64::new(0),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn working_path(&self, handle: &ScratchHandle) -> PathBuf {
        PathBuf::from(handle.as_str())
    }

    fn final_path(&self, trace_id: &TraceId) -> PathBuf {
        self.root.join(trace_id.as_str())
    }

    fn resolve_existing(&self, handle: &ScratchHandle, rel: &str) -> Result<PathBuf, String> {
        let root = self
            .working_path(handle)
            .canonicalize()
            .map_err(|e| format!("scratch root is not available: {e}"))?;
        crate::jail::resolve_existing(&root, rel, "scratch")
    }

    fn resolve_for_write(&self, handle: &ScratchHandle, rel: &str) -> Result<PathBuf, String> {
        let root = self
            .working_path(handle)
            .canonicalize()
            .map_err(|e| format!("scratch root is not available: {e}"))?;
        crate::jail::resolve_for_write(&root, rel, "scratch")
    }
}

#[async_trait]
impl ScratchBackend for FsScratch {
    async fn prepare(&self, parent: Option<&TraceId>) -> Result<ScratchHandle, std::io::Error> {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let working = self
            .root
            .join(PENDING_DIRNAME)
            .join(format!("{}-{n}", std::process::id()));
        if working.exists() {
            fs::remove_dir_all(&working)?;
        }
        if let Some(parent_id) = parent {
            let parent_scratch = self.final_path(parent_id);
            if parent_scratch.exists() {
                // Seed working state but not `out/`: those files were already
                // delivered as the parent's outbound blobs, and re-seeding them
                // would make every follow-up re-emit its ancestors' outputs.
                copy_tree_excluding_top(&parent_scratch, &working, crate::blobs::OUT_DIRNAME)?;
            } else {
                fs::create_dir_all(&working)?;
            }
        } else {
            fs::create_dir_all(&working)?;
        }
        Ok(ScratchHandle::new(working.display().to_string()))
    }

    async fn finalize(
        &self,
        handle: &ScratchHandle,
        trace_id: &TraceId,
    ) -> Result<(), std::io::Error> {
        let final_dir = self.final_path(trace_id);
        if final_dir.exists() {
            fs::remove_dir_all(&final_dir)?;
        }
        if let Some(parent) = final_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(self.working_path(handle), final_dir)?;
        Ok(())
    }

    async fn write_bytes(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
        let path = self.resolve_for_write(handle, rel_path)?;
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| format!("failed to replace existing scratch file {rel_path}: {e}"))?;
        }
        fs::write(&path, bytes).map_err(|e| format!("failed to write {rel_path}: {e}"))?;
        let root = self
            .working_path(handle)
            .canonicalize()
            .map_err(|e| format!("scratch root is not available: {e}"))?;
        Ok(crate::jail::rel_path_from(&root, &path))
    }

    async fn read_bytes(&self, handle: &ScratchHandle, rel_path: &str) -> Result<Vec<u8>, String> {
        let path = self.resolve_existing(handle, rel_path)?;
        if !path.is_file() {
            return Err(format!("scratch_read path is not a file: {rel_path}"));
        }
        fs::read(&path).map_err(|e| format!("failed to read {rel_path}: {e}"))
    }

    async fn import_file(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
        source: &Path,
    ) -> Result<String, String> {
        let path = self.resolve_for_write(handle, rel_path)?;
        if path.exists() {
            fs::remove_file(&path)
                .map_err(|e| format!("failed to replace existing scratch file {rel_path}: {e}"))?;
        }
        match fs::hard_link(source, &path) {
            Ok(()) => {}
            Err(_) => {
                fs::copy(source, &path).map_err(|e| format!("failed to import {rel_path}: {e}"))?;
            }
        }
        let root = self
            .working_path(handle)
            .canonicalize()
            .map_err(|e| format!("scratch root is not available: {e}"))?;
        Ok(crate::jail::rel_path_from(&root, &path))
    }

    async fn local_path(&self, handle: &ScratchHandle, rel_path: &str) -> Option<PathBuf> {
        self.resolve_existing(handle, rel_path).ok()
    }

    async fn list(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
    ) -> Result<Vec<ScratchEntry>, String> {
        let start = self.resolve_existing(handle, rel_path)?;
        if !start.is_dir() {
            return Err(format!("scratch_list path is not a directory: {rel_path}"));
        }
        let root = self
            .working_path(handle)
            .canonicalize()
            .map_err(|e| format!("scratch root is not available: {e}"))?;
        let mut read_dir = fs::read_dir(&start)
            .map_err(|e| format!("failed to list {rel_path}: {e}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("failed to list {rel_path}: {e}"))?;
        read_dir.sort_by_key(|entry| entry.file_name());
        let mut entries = Vec::new();
        for entry in read_dir {
            let path = entry.path();
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            entries.push(ScratchEntry {
                path: crate::jail::rel_path_from(&root, &path),
                kind: if metadata.is_dir() {
                    ScratchEntryKind::Dir
                } else {
                    ScratchEntryKind::File
                },
                bytes: metadata.is_file().then_some(metadata.len()),
            });
        }
        Ok(entries)
    }
}

#[derive(Debug, Default)]
pub struct MemScratch {
    next: AtomicU64,
    working: Mutex<HashMap<String, BTreeMap<String, Vec<u8>>>>,
    finalized: Mutex<HashMap<String, BTreeMap<String, Vec<u8>>>>,
}

impl MemScratch {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ScratchBackend for MemScratch {
    async fn prepare(&self, parent: Option<&TraceId>) -> Result<ScratchHandle, std::io::Error> {
        let n = self.next.fetch_add(1, Ordering::SeqCst);
        let handle = ScratchHandle::new(format!("mem-{n}"));
        let out_prefix = format!("{}/", crate::blobs::OUT_DIRNAME);
        // Same rule as `FsScratch`: never seed `out/` into a follow-up.
        let seed: BTreeMap<String, Vec<u8>> = parent
            .and_then(|id| self.finalized.lock().unwrap().get(id.as_str()).cloned())
            .unwrap_or_default()
            .into_iter()
            .filter(|(path, _)| !path.starts_with(&out_prefix))
            .collect();
        self.working
            .lock()
            .unwrap()
            .insert(handle.as_str().to_string(), seed);
        Ok(handle)
    }

    async fn finalize(
        &self,
        handle: &ScratchHandle,
        trace_id: &TraceId,
    ) -> Result<(), std::io::Error> {
        let mut working = self.working.lock().unwrap();
        let tree = working.remove(handle.as_str()).unwrap_or_default();
        self.finalized
            .lock()
            .unwrap()
            .insert(trace_id.as_str().to_string(), tree);
        Ok(())
    }

    async fn write_bytes(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
        bytes: &[u8],
    ) -> Result<String, String> {
        let rel = normalize_rel(rel_path, false)?;
        self.working
            .lock()
            .unwrap()
            .entry(handle.as_str().to_string())
            .or_default()
            .insert(rel.clone(), bytes.to_vec());
        Ok(rel)
    }

    async fn read_bytes(&self, handle: &ScratchHandle, rel_path: &str) -> Result<Vec<u8>, String> {
        let rel = normalize_rel(rel_path, false)?;
        self.working
            .lock()
            .unwrap()
            .get(handle.as_str())
            .and_then(|tree| tree.get(&rel).cloned())
            .ok_or_else(|| format!("path does not exist inside scratch root: {rel_path}"))
    }

    async fn list(
        &self,
        handle: &ScratchHandle,
        rel_path: &str,
    ) -> Result<Vec<ScratchEntry>, String> {
        let rel = normalize_rel(rel_path, true)?;
        let prefix = if rel.is_empty() {
            String::new()
        } else {
            format!("{rel}/")
        };
        let guard = self.working.lock().unwrap();
        let tree = guard.get(handle.as_str()).cloned().unwrap_or_default();
        if !rel.is_empty()
            && !tree.contains_key(&rel)
            && !tree.keys().any(|p| p.starts_with(&prefix))
        {
            return Err(format!(
                "path does not exist inside scratch root: {rel_path}"
            ));
        }
        if tree.contains_key(&rel) {
            return Err(format!("scratch_list path is not a directory: {rel_path}"));
        }
        let mut dirs = BTreeSet::new();
        let mut files = BTreeMap::new();
        for (path, bytes) in tree {
            let Some(rest) = path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            if let Some((dir, _)) = rest.split_once('/') {
                dirs.insert(format!("{prefix}{dir}").trim_matches('/').to_string());
            } else {
                files.insert(path, bytes.len() as u64);
            }
        }
        let mut entries = Vec::new();
        for path in dirs {
            entries.push(ScratchEntry {
                path,
                kind: ScratchEntryKind::Dir,
                bytes: None,
            });
        }
        for (path, len) in files {
            entries.push(ScratchEntry {
                path,
                kind: ScratchEntryKind::File,
                bytes: Some(len),
            });
        }
        Ok(entries)
    }
}

/// Schemas for the built-in scratchpad tools. Used by `Agent::describe`
/// without needing to construct a per-ask scratch jail.
pub(crate) fn scratch_tool_schemas() -> Vec<ToolSchema> {
    vec![
        scratch_write_schema(),
        scratch_read_schema(),
        scratch_list_schema(),
    ]
}

/// Recursively copy `from` into `to` (used by the filesystem backend to seed a
/// fork/resume from its parent's finalized subtree).
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
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
    }
    Ok(())
}

fn copy_tree_excluding_top(from: &Path, to: &Path, exclude: &str) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if entry.file_name() == exclude {
            continue;
        }
        let file_type = entry.file_type()?;
        let src = entry.path();
        let dst = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&src, &dst)?;
        } else if file_type.is_file() {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

struct ScratchWrite {
    session: ScratchSession,
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
        match self.session.write_bytes(rel, content.as_bytes()).await {
            Ok(path) => Ok(json!({
                "path": path,
                "bytes_written": content.len(),
            })),
            Err(error) => Err(json!({ "error": error })),
        }
    }
}

struct ScratchRead {
    session: ScratchSession,
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
        match self.session.read_bytes(rel).await {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => Ok(json!({ "path": rel, "content": content })),
                Err(err) => {
                    Err(json!({ "error": format!("scratch_read path is not UTF-8: {err}") }))
                }
            },
            Err(error) => Err(json!({ "error": error })),
        }
    }
}

struct ScratchList {
    session: ScratchSession,
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
        match self.session.list(rel).await {
            Ok(entries) => Ok(json!({
                "entries": entries.into_iter().map(|entry| json!({
                    "path": entry.path,
                    "kind": match entry.kind {
                        ScratchEntryKind::File => "file",
                        ScratchEntryKind::Dir => "dir",
                    },
                    "bytes": entry.bytes,
                })).collect::<Vec<_>>()
            })),
            Err(error) => Err(json!({ "error": error })),
        }
    }
}

fn normalize_rel(rel: &str, allow_empty: bool) -> Result<String, String> {
    let rel = rel.trim();
    if rel.is_empty() {
        return if allow_empty {
            Ok(String::new())
        } else {
            Err("path must name a file, not the scratch root".to_string())
        };
    }
    let path = Path::new(rel);
    if path.is_absolute() {
        return Err("path must be relative to scratch root".to_string());
    }
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path escapes scratch root: {rel}"));
            }
        }
    }
    if parts.is_empty() {
        return if allow_empty {
            Ok(String::new())
        } else {
            Err("path must name a file, not the scratch root".to_string())
        };
    }
    Ok(parts.join("/"))
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
