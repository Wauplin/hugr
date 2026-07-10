//! `fs_read` — a root-jailed, read-only filesystem tool family. Generalized
//! from the `hugr-docs` retrieval tools: the docs-specific `AI_INDEX`/`is_index`
//! bits are dropped and the root is a manifest-configured scope.
//!
//! One grant registers a family of six read capabilities, all sharing the same
//! [`FsRoot`] jail:
//!
//! | tool             | purpose                                             |
//! | ---------------- | --------------------------------------------------- |
//! | `fs_list`        | list files/dirs (optionally recursive)              |
//! | `fs_search`      | case-insensitive substring search over text files   |
//! | `fs_read`        | read one text file (byte-capped)                    |
//! | `fs_read_range`  | read a 1-based inclusive line range                 |
//! | `fs_read_many`   | read several files in one call                      |
//! | `fs_outline`     | markdown-style heading outline                      |
//!
//! Privilege class: **read-only** (`requires_permission() == false`) — the jail
//! is the boundary. Every path is canonicalized and must resolve under the
//! root; absolute paths and any `..`/root/prefix component are rejected, so a
//! symlink cannot escape (the canonical target is re-checked against the root).

use std::collections::VecDeque;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde_json::json;

const DEFAULT_READ_LIMIT_BYTES: usize = 200_000;
const MAX_READ_LIMIT_BYTES: usize = 1_000_000;
const DEFAULT_SEARCH_LIMIT_BYTES: u64 = 512_000;
const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_MAX_MATCHES: usize = 50;
const DEFAULT_RANGE_MAX_LINES: usize = 200;
const MAX_RANGE_LINES: usize = 5_000;
const MAX_BATCH_READS: usize = 50;
const DEFAULT_OUTLINE_MAX_DOCUMENTS: usize = 100;
const DEFAULT_OUTLINE_MAX_HEADINGS: usize = 1_000;
const WALK_CEILING: usize = 20_000;

/// A canonicalized read-only root. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct FsRoot {
    root: Arc<PathBuf>,
}

impl FsRoot {
    /// Canonicalize and validate a root directory.
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing fs_read root {}", root.as_ref().display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "fs_read root is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root: Arc::new(root),
        })
    }

    /// The six read capabilities backed by this root.
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(FsList(self.clone())),
            Arc::new(FsSearch(self.clone())),
            Arc::new(FsRead(self.clone())),
            Arc::new(FsReadRange(self.clone())),
            Arc::new(FsReadMany(self.clone())),
            Arc::new(FsOutline(self.clone())),
        ]
    }

    fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// Resolve a caller-supplied relative path to an existing canonical path
    /// inside the jail, or error. `None`/empty ⇒ the root itself.
    fn resolve_existing(&self, rel: Option<&str>) -> Result<PathBuf> {
        let rel = rel.unwrap_or("").trim();
        let candidate = if rel.is_empty() {
            self.root().to_path_buf()
        } else {
            let path = Path::new(rel);
            anyhow::ensure!(
                !path.is_absolute(),
                "path must be relative to the tool root"
            );
            for component in path.components() {
                match component {
                    Component::Normal(_) | Component::CurDir => {}
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        return Err(anyhow!("path escapes the tool root"));
                    }
                }
            }
            self.root().join(path)
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("path does not exist inside the tool root: {rel}"))?;
        anyhow::ensure!(
            canonical.starts_with(self.root()),
            "path escapes the tool root: {rel}"
        );
        Ok(canonical)
    }

    fn rel_path(&self, path: &Path) -> Result<String> {
        let rel = path
            .strip_prefix(self.root())
            .with_context(|| format!("path {} is not under the tool root", path.display()))?;
        Ok(path_to_slash(rel))
    }
}

fn path_to_slash(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn looks_textual(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "md" | "mdx" | "txt" | "rst" | "adoc" | "json" | "yaml" | "yml" | "toml" | "csv"
    )
}

fn read_utf8_prefix(path: &Path, limit: usize) -> Result<(String, bool)> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let truncated = bytes.len() > limit;
    let slice = if truncated { &bytes[..limit] } else { &bytes };
    Ok((String::from_utf8_lossy(slice).into_owned(), truncated))
}

fn truncate_utf8_prefix(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn walk_files(root: &FsRoot, start: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([start.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let mut entries = fs::read_dir(&dir)
            .with_context(|| format!("listing {}", dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("reading directory entries for {}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let canonical = match path.canonicalize() {
                Ok(path) if path.starts_with(root.root()) => path,
                _ => continue,
            };
            let metadata = match fs::metadata(&canonical) {
                Ok(metadata) => metadata,
                Err(_) => continue,
            };
            if metadata.is_dir() {
                queue.push_back(canonical);
            } else if metadata.is_file() {
                out.push(canonical);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

fn markdown_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = line.trim_start();
    let level = trimmed.chars().take_while(|ch| *ch == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let text = rest.trim().trim_end_matches('#').trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some((level, text))
}

/// Wrap a fallible impl as the standard tool result: `Ok`/`Err(error)` both
/// become tool results the model reads.
fn wrap(result: Result<Value>) -> std::result::Result<Value, Value> {
    result.map_err(|error| json!({ "error": error.to_string() }))
}

macro_rules! read_tool {
    ($ty:ident, $name:literal) => {
        struct $ty(FsRoot);
    };
}

read_tool!(FsList, "fs_list");
read_tool!(FsSearch, "fs_search");
read_tool!(FsRead, "fs_read");
read_tool!(FsReadRange, "fs_read_range");
read_tool!(FsReadMany, "fs_read_many");
read_tool!(FsOutline, "fs_outline");

#[async_trait]
impl Capability for FsList {
    fn name(&self) -> &str {
        "fs_list"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_list",
            "List files and directories under the tool root. Paths are relative to the root.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative directory path. Defaults to the root." },
                    "recursive": { "type": "boolean", "description": "Whether to list recursively. Defaults to false." },
                    "max_entries": { "type": "integer", "minimum": 1, "maximum": 2000, "description": "Maximum entries to return." }
                },
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(list_impl(&self.0, args))
    }
}

fn list_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let path = args.get("path").and_then(Value::as_str);
    let recursive = args
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_entries = args
        .get("max_entries")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LIST_LIMIT as u64)
        .clamp(1, 2000) as usize;
    let start = root.resolve_existing(path)?;
    anyhow::ensure!(start.is_dir(), "fs_list path must be a directory");

    let paths = if recursive {
        walk_files(root, &start, max_entries)?
    } else {
        let mut entries = fs::read_dir(&start)
            .with_context(|| format!("listing {}", start.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("reading directory entries for {}", start.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        entries
            .into_iter()
            .take(max_entries)
            .filter_map(|entry| entry.path().canonicalize().ok())
            .filter(|path| path.starts_with(root.root()))
            .collect()
    };

    let mut entries = Vec::new();
    for path in paths {
        let metadata = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
        entries.push(json!({
            "path": root.rel_path(&path)?,
            "kind": if metadata.is_dir() { "dir" } else { "file" },
            "bytes": if metadata.is_file() { Some(metadata.len()) } else { None },
        }));
    }
    let truncated = entries.len() >= max_entries;
    Ok(json!({ "entries": entries, "truncated": truncated }))
}

#[async_trait]
impl Capability for FsRead {
    fn name(&self) -> &str {
        "fs_read"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_read",
            "Read one text file under the tool root. Read-only; cannot access paths outside the root.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the tool root." },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": 1000000, "description": "Maximum bytes to return. Defaults to 200000." }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_read requires string `path`")?;
            let limit = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_READ_LIMIT_BYTES as u64)
                .clamp(1, MAX_READ_LIMIT_BYTES as u64) as usize;
            read_document(&self.0, rel, limit)
        })())
    }
}

fn read_document(root: &FsRoot, rel: &str, limit: usize) -> Result<Value> {
    let path = root.resolve_existing(Some(rel))?;
    anyhow::ensure!(path.is_file(), "fs_read path must be a file");
    let rel = root.rel_path(&path)?;
    let (content, truncated) = read_utf8_prefix(&path, limit)?;
    Ok(json!({
        "path": rel,
        "bytes_returned": content.len(),
        "truncated": truncated,
        "content": content,
    }))
}

#[async_trait]
impl Capability for FsReadRange {
    fn name(&self) -> &str {
        "fs_read_range"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_read_range",
            "Read a line range from one text file under the tool root. Lines are 1-based, inclusive.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the tool root." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "First line to read, 1-based." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read, inclusive. If omitted, max_lines controls the window." },
                    "max_lines": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum lines when end_line is omitted or too large. Defaults to 200." },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": 1000000, "description": "Maximum bytes of content to return. Defaults to 200000." }
                },
                "required": ["path", "start_line"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap((|| {
            let rel = args
                .get("path")
                .and_then(Value::as_str)
                .context("fs_read_range requires string `path`")?;
            let start_line = args
                .get("start_line")
                .and_then(Value::as_u64)
                .context("fs_read_range requires integer `start_line`")?
                as usize;
            let end_line = args
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|l| l as usize);
            let max_lines = args
                .get("max_lines")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_RANGE_MAX_LINES as u64)
                .clamp(1, MAX_RANGE_LINES as u64) as usize;
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_READ_LIMIT_BYTES as u64)
                .clamp(1, MAX_READ_LIMIT_BYTES as u64) as usize;
            read_range_document(&self.0, rel, start_line, end_line, max_lines, max_bytes)
        })())
    }
}

fn read_range_document(
    root: &FsRoot,
    rel: &str,
    start_line: usize,
    end_line: Option<usize>,
    max_lines: usize,
    max_bytes: usize,
) -> Result<Value> {
    anyhow::ensure!(start_line >= 1, "start_line must be at least 1");
    if let Some(end_line) = end_line {
        anyhow::ensure!(end_line >= start_line, "end_line must be >= start_line");
    }
    let path = root.resolve_existing(Some(rel))?;
    anyhow::ensure!(path.is_file(), "fs_read_range path must be a file");
    let rel = root.rel_path(&path)?;
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes);
    let lines = content.lines().collect::<Vec<_>>();
    let requested_end = end_line.unwrap_or_else(|| start_line.saturating_add(max_lines - 1));
    let capped_end = requested_end.min(start_line.saturating_add(max_lines - 1));
    let selected = if start_line > lines.len() {
        Vec::new()
    } else {
        lines[(start_line - 1)..lines.len().min(capped_end)].to_vec()
    };
    let line_truncated =
        requested_end > capped_end || (end_line.is_none() && capped_end < lines.len());
    let end_line_returned = if selected.is_empty() {
        start_line.saturating_sub(1)
    } else {
        start_line + selected.len() - 1
    };
    let (content, byte_truncated) = truncate_utf8_prefix(&selected.join("\n"), max_bytes);
    Ok(json!({
        "path": rel,
        "start_line": start_line,
        "end_line": end_line_returned,
        "total_lines": lines.len(),
        "bytes_returned": content.len(),
        "truncated": line_truncated || byte_truncated,
        "content": content,
    }))
}

#[async_trait]
impl Capability for FsReadMany {
    fn name(&self) -> &str {
        "fs_read_many"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_read_many",
            "Read multiple text files under the tool root in one call.",
            json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 50, "description": "File paths relative to the tool root." },
                    "max_bytes_per_document": { "type": "integer", "minimum": 1, "maximum": 1000000, "description": "Maximum bytes per file. Defaults to 200000." },
                    "max_documents": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum files to read. Defaults to 50." }
                },
                "required": ["paths"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(read_many_impl(&self.0, args))
    }
}

fn read_many_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .context("fs_read_many requires array `paths`")?;
    anyhow::ensure!(!paths.is_empty(), "fs_read_many paths cannot be empty");
    let max_documents = args
        .get("max_documents")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_BATCH_READS as u64)
        .clamp(1, MAX_BATCH_READS as u64) as usize;
    let max_bytes = args
        .get("max_bytes_per_document")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_LIMIT_BYTES as u64)
        .clamp(1, MAX_READ_LIMIT_BYTES as u64) as usize;
    let mut documents = Vec::new();
    let mut errors = Vec::new();
    for path in paths.iter().take(max_documents) {
        let Some(rel) = path.as_str() else {
            errors.push(json!({ "path": path, "error": "path must be a string" }));
            continue;
        };
        match read_document(root, rel, max_bytes) {
            Ok(document) => documents.push(document),
            Err(error) => errors.push(json!({ "path": rel, "error": error.to_string() })),
        }
    }
    Ok(json!({
        "documents": documents,
        "errors": errors,
        "truncated": paths.len() > max_documents,
    }))
}

#[async_trait]
impl Capability for FsSearch {
    fn name(&self) -> &str {
        "fs_search"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_search",
            "Search text files under the tool root for a case-insensitive substring. Returns snippets with relative paths and line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring to search for." },
                    "path": { "type": "string", "description": "Optional relative directory or file to search within." },
                    "max_matches": { "type": "integer", "minimum": 1, "maximum": 500, "description": "Maximum matches to return. Defaults to 50." }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(search_impl(&self.0, args))
    }
}

fn search_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .context("fs_search requires string `query`")?
        .trim();
    anyhow::ensure!(!query.is_empty(), "fs_search query cannot be empty");
    let query_lower = query.to_ascii_lowercase();
    let max_matches = args
        .get("max_matches")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_MATCHES as u64)
        .clamp(1, 500) as usize;
    let start = root.resolve_existing(args.get("path").and_then(Value::as_str))?;
    let files = if start.is_file() {
        vec![start]
    } else {
        walk_files(root, &start, WALK_CEILING)?
    };
    let mut matches = Vec::new();
    let mut searched_files = 0usize;
    for file in files {
        let metadata = match fs::metadata(&file) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        if !looks_textual(&file) || metadata.len() > DEFAULT_SEARCH_LIMIT_BYTES {
            continue;
        }
        searched_files += 1;
        let (content, _) = read_utf8_prefix(&file, DEFAULT_SEARCH_LIMIT_BYTES as usize)?;
        let rel = root.rel_path(&file)?;
        for (line_idx, line) in content.lines().enumerate() {
            if line.to_ascii_lowercase().contains(&query_lower) {
                matches.push(json!({
                    "path": rel,
                    "line": line_idx + 1,
                    "snippet": line.trim(),
                }));
                if matches.len() >= max_matches {
                    return Ok(json!({
                        "query": query,
                        "matches": matches,
                        "searched_files": searched_files,
                        "truncated": true,
                    }));
                }
            }
        }
    }
    Ok(json!({
        "query": query,
        "matches": matches,
        "searched_files": searched_files,
        "truncated": false,
    }))
}

#[async_trait]
impl Capability for FsOutline {
    fn name(&self) -> &str {
        "fs_outline"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_outline",
            "Return markdown-style headings for one text file or for text files under a directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional relative file or directory path. Defaults to the root." },
                    "max_documents": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum text files to inspect. Defaults to 100." },
                    "max_headings": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum headings across all inspected files. Defaults to 1000." }
                },
                "additionalProperties": false
            }),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(outline_impl(&self.0, args))
    }
}

fn outline_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let start = root.resolve_existing(args.get("path").and_then(Value::as_str))?;
    let max_documents = args
        .get("max_documents")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_OUTLINE_MAX_DOCUMENTS as u64)
        .clamp(1, 1_000) as usize;
    let max_headings = args
        .get("max_headings")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_OUTLINE_MAX_HEADINGS as u64)
        .clamp(1, 5_000) as usize;
    let start_is_file = start.is_file();
    let candidates = if start_is_file {
        vec![start]
    } else {
        walk_files(root, &start, WALK_CEILING)?
    };
    let mut documents = Vec::new();
    let mut searched_files = 0usize;
    let mut heading_count = 0usize;
    let mut hit_document_limit = false;
    let mut hit_heading_limit = false;

    for file in candidates {
        if !start_is_file && searched_files >= max_documents {
            hit_document_limit = true;
            break;
        }
        let metadata = match fs::metadata(&file) {
            Ok(metadata) if metadata.is_file() => metadata,
            _ => continue,
        };
        if !looks_textual(&file) || metadata.len() > DEFAULT_SEARCH_LIMIT_BYTES {
            continue;
        }
        searched_files += 1;
        let (content, _) = read_utf8_prefix(&file, DEFAULT_SEARCH_LIMIT_BYTES as usize)?;
        let rel = root.rel_path(&file)?;
        let mut headings = Vec::new();
        for (line_idx, line) in content.lines().enumerate() {
            let Some((level, text)) = markdown_heading(line) else {
                continue;
            };
            if heading_count >= max_headings {
                hit_heading_limit = true;
                break;
            }
            heading_count += 1;
            headings.push(json!({ "line": line_idx + 1, "level": level, "text": text }));
        }
        if start_is_file || !headings.is_empty() {
            documents.push(json!({ "path": rel, "headings": headings }));
        }
        if hit_heading_limit {
            break;
        }
    }
    Ok(json!({
        "documents": documents,
        "searched_files": searched_files,
        "truncated": hit_document_limit || hit_heading_limit,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal self-cleaning temp dir (the repo avoids the `tempfile` dep).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "hugr-fsread-{tag}-{}-{:p}",
                std::process::id(),
                &tag as *const _
            ));
            fs::create_dir_all(&base).unwrap();
            TmpDir(base)
        }
        fn write(&self, rel: &str, contents: &str) {
            let path = self.0.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn root(tag: &str) -> (TmpDir, FsRoot) {
        let dir = TmpDir::new(tag);
        dir.write("a.md", "# Title\nhello world\nsecond line\n");
        dir.write("sub/b.txt", "needle here\nother\n");
        let fs_root = FsRoot::new(&dir.0).unwrap();
        (dir, fs_root)
    }

    #[test]
    fn lists_reads_searches_and_outlines() {
        let (_dir, root) = root("basic");
        let listed = list_impl(&root, json!({ "recursive": true })).unwrap();
        let paths: Vec<_> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"a.md".to_string()));
        assert!(paths.contains(&"sub/b.txt".to_string()));

        let read = read_document(&root, "a.md", 1_000_000).unwrap();
        assert!(read["content"].as_str().unwrap().contains("hello world"));

        let searched = search_impl(&root, json!({ "query": "needle" })).unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
        assert_eq!(searched["matches"][0]["path"], "sub/b.txt");

        let outline = outline_impl(&root, json!({ "path": "a.md" })).unwrap();
        assert_eq!(outline["documents"][0]["headings"][0]["text"], "Title");

        let ranged = read_range_document(&root, "a.md", 2, Some(2), 200, 1_000_000).unwrap();
        assert_eq!(ranged["content"], "hello world");
    }

    #[test]
    fn jail_rejects_traversal_and_absolute_paths() {
        let (_dir, root) = root("jail");
        assert!(root.resolve_existing(Some("../secret")).is_err());
        assert!(root.resolve_existing(Some("/etc/passwd")).is_err());
        assert!(read_document(&root, "../a.md", 1000).is_err());
        // A legitimate in-jail path still resolves.
        assert!(root.resolve_existing(Some("sub/b.txt")).is_ok());
    }

    /// A symlink inside the jail that points outside it must not be a read
    /// primitive for the target. Path components are all `Normal`, so the
    /// traversal check passes — the post-canonicalize `starts_with(root)`
    /// re-check is what rejects it.
    #[cfg(unix)]
    #[test]
    fn jail_rejects_symlink_that_escapes_the_root() {
        let (dir, root) = root("symlink");
        // A secret file living OUTSIDE the jail root.
        let outside = dir.0.parent().unwrap().join(format!(
            "hugr-fsread-secret-{}-{:p}",
            std::process::id(),
            &dir as *const _
        ));
        fs::write(&outside, "top secret").unwrap();

        // A symlink inside the root pointing at that outside file.
        let link = dir.0.join("escape.md");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        // The symlink's own path has only Normal components, so it clears the
        // component check — but canonicalization resolves it to `outside`,
        // which fails the starts_with(root) re-check.
        let err = root.resolve_existing(Some("escape.md")).unwrap_err();
        assert!(err.to_string().contains("escapes the tool root"), "{err}");
        assert!(read_document(&root, "escape.md", 1000).is_err());

        let _ = fs::remove_file(&outside);
    }
}
