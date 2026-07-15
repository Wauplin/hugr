//! `fs_read` — a root-jailed, read-only filesystem tool family. Generalized
//! from the `huglet-docs` retrieval tools: the docs-specific `AI_INDEX`/`is_index`
//! bits are dropped and the root is a manifest-configured scope.
//!
//! One grant registers a family of eight read capabilities over one or more
//! named [`FsRoot`] jails. Files are always addressed as `<root-name>/<path>`;
//! a tree operation with no path spans every root, and `fs_list` with no path
//! lists the root names. The family:
//!
//! | tool             | purpose                                             |
//! | ---------------- | --------------------------------------------------- |
//! | `fs_list`        | list files/dirs (optionally recursive)              |
//! | `fs_search`      | case-insensitive substring search over text files   |
//! | `fs_grep`        | regular-expression search over text files            |
//! | `fs_glob`        | match paths with a glob pattern                      |
//! | `fs_read`        | read one text file (byte-capped)                    |
//! | `fs_read_range`  | read a 1-based inclusive line range                 |
//! | `fs_read_many`   | read several files in one call                      |
//! | `fs_outline`     | markdown-style heading outline                      |
//!
//! Privilege class: **read-only** (`requires_permission() == false`) — the jail
//! is the boundary. Every path is canonicalized and must resolve under the
//! root; absolute paths and any `..`/root/prefix component are rejected, so a
//! symlink cannot escape (the canonical target is re-checked against the root).

use std::collections::{HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use regex::RegexBuilder;
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

/// A single canonicalized directory jail. Path resolution and the
/// post-canonicalize `starts_with` re-check live here; [`FsRoot`] composes one
/// or more of these under names.
#[derive(Debug)]
struct Jail {
    root: PathBuf,
}

impl Jail {
    fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing fs root {}", root.as_ref().display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "fs root is not a directory: {}",
            root.display()
        );
        Ok(Self { root })
    }

    fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn contains(&self, path: &Path) -> bool {
        path.starts_with(&self.root)
    }

    /// Resolve a jail-relative path to an existing canonical path inside the
    /// jail, or error. `None`/empty ⇒ the jail root itself.
    fn resolve_existing(&self, rel: Option<&str>) -> Result<PathBuf> {
        let rel = rel.unwrap_or("").trim();
        let candidate = if rel.is_empty() {
            self.root.clone()
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
            self.root.join(path)
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("path does not exist inside the tool root: {rel}"))?;
        anyhow::ensure!(
            canonical.starts_with(&self.root),
            "path escapes the tool root: {rel}"
        );
        Ok(canonical)
    }

    fn rel_path(&self, path: &Path) -> Result<String> {
        let rel = path
            .strip_prefix(&self.root)
            .with_context(|| format!("path {} is not under the tool root", path.display()))?;
        Ok(path_to_slash(rel))
    }
}

#[derive(Debug)]
struct NamedJail {
    name: String,
    jail: Jail,
}

/// One or more named directory jails backing the read capabilities. Callers
/// address files as `<root-name>/<path>`, and a tree operation with no path
/// spans every root. Cheap to clone (`Arc` inside).
#[derive(Clone, Debug)]
pub struct FsRoot {
    jails: Arc<Vec<NamedJail>>,
}

impl FsRoot {
    /// Build from named roots. Every root is addressed as `<name>/<path>`;
    /// names must be unique and free of `/`.
    pub fn with_named(roots: Vec<(String, PathBuf)>) -> Result<Self> {
        anyhow::ensure!(!roots.is_empty(), "fs_read requires at least one root");
        let mut jails = Vec::with_capacity(roots.len());
        let mut seen = HashSet::new();
        for (name, path) in roots {
            anyhow::ensure!(!name.is_empty(), "root name cannot be empty");
            anyhow::ensure!(!name.contains('/'), "root name `{name}` cannot contain `/`");
            anyhow::ensure!(seen.insert(name.clone()), "duplicate root name `{name}`");
            let jail = Jail::new(&path)?;
            jails.push(NamedJail { name, jail });
        }
        Ok(Self {
            jails: Arc::new(jails),
        })
    }

    /// The eight read capabilities backed by these roots.
    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(FsList(self.clone())),
            Arc::new(FsSearch(self.clone())),
            Arc::new(FsGrep(self.clone())),
            Arc::new(FsGlob(self.clone())),
            Arc::new(FsRead(self.clone())),
            Arc::new(FsReadRange(self.clone())),
            Arc::new(FsReadMany(self.clone())),
            Arc::new(FsOutline(self.clone())),
        ]
    }

    fn root_names(&self) -> Vec<&str> {
        self.jails.iter().map(|j| j.name.as_str()).collect()
    }

    /// Split a caller path into (jail, jail-relative sub-path). The first
    /// segment always names a root; the remainder is relative within it.
    fn locate(&self, rel: &str) -> Result<(&NamedJail, Option<String>)> {
        let rel = rel.trim().trim_start_matches('/');
        let (name, sub) = match rel.split_once('/') {
            Some((name, sub)) => (name, sub),
            None => (rel, ""),
        };
        anyhow::ensure!(
            !name.is_empty(),
            "path must name a root as `<root>/<path>` (roots: {})",
            self.root_names().join(", ")
        );
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
        let sub = sub.trim();
        Ok((jail, (!sub.is_empty()).then(|| sub.to_string())))
    }

    /// Resolve a caller path to an existing canonical path inside its jail.
    fn resolve_existing(&self, rel: Option<&str>) -> Result<PathBuf> {
        let (jail, sub) = self.locate(rel.unwrap_or(""))?;
        jail.jail.resolve_existing(sub.as_deref())
    }

    /// Caller-facing display path for a canonical path: always `<name>/<rel>`
    /// (just `<name>` for a root itself).
    fn rel_path(&self, path: &Path) -> Result<String> {
        let owner = self
            .jails
            .iter()
            .find(|j| j.jail.contains(path))
            .with_context(|| format!("path {} is not under any tool root", path.display()))?;
        let rel = owner.jail.rel_path(path)?;
        Ok(if rel.is_empty() {
            owner.name.clone()
        } else {
            format!("{}/{rel}", owner.name)
        })
    }

    /// The (jail, start) pairs a tree operation should walk. `None`/empty ⇒
    /// every root; a given path ⇒ the single resolved target within its root.
    fn walk_targets(&self, path: Option<&str>) -> Result<Vec<(&Jail, PathBuf)>> {
        match path.map(str::trim).filter(|p| !p.is_empty()) {
            None => Ok(self
                .jails
                .iter()
                .map(|j| (&j.jail, j.jail.root().to_path_buf()))
                .collect()),
            Some(rel) => {
                let (jail, sub) = self.locate(rel)?;
                let resolved = jail.jail.resolve_existing(sub.as_deref())?;
                Ok(vec![(&jail.jail, resolved)])
            }
        }
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
    let file = fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut bytes = Vec::with_capacity(limit.saturating_add(1));
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
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

fn walk_files(jail: &Jail, start: &Path, limit: usize) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([start.to_path_buf()]);
    let mut visited = HashSet::new();
    while let Some(dir) = queue.pop_front() {
        let canonical_dir = dir.canonicalize()?;
        if !visited.insert(canonical_dir.clone()) {
            continue;
        }
        anyhow::ensure!(
            visited.len() <= limit.saturating_mul(4).max(64),
            "directory visit limit exceeded"
        );
        let mut entries = fs::read_dir(&canonical_dir)
            .with_context(|| format!("listing {}", canonical_dir.display()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .with_context(|| format!("reading directory entries for {}", dir.display()))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let canonical = match path.canonicalize() {
                Ok(path) if jail.contains(&path) => path,
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
read_tool!(FsGrep, "fs_grep");
read_tool!(FsGlob, "fs_glob");
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
            "List files and directories. Paths are addressed as `<root>/<path>`; with no path, this lists the available roots.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory as `<root>/<path>`. Omit to list the roots themselves." },
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
    let has_path = path.map(str::trim).is_some_and(|p| !p.is_empty());
    let recursive = args
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let max_entries = args
        .get("max_entries")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LIST_LIMIT as u64)
        .clamp(1, 2000) as usize;

    // With no path, list the roots themselves as top-level dirs.
    if !has_path && !recursive {
        let entries: Vec<_> = root
            .jails
            .iter()
            .map(|j| json!({ "path": j.name, "kind": "dir", "bytes": Value::Null }))
            .collect();
        return Ok(json!({ "entries": entries, "truncated": false }));
    }

    let mut paths = Vec::new();
    for (jail, start) in root.walk_targets(path)? {
        anyhow::ensure!(start.is_dir(), "fs_list path must be a directory");
        if recursive {
            let remaining = max_entries.saturating_sub(paths.len());
            paths.extend(walk_files(jail, &start, remaining.max(1))?);
        } else {
            let mut entries = fs::read_dir(&start)
                .with_context(|| format!("listing {}", start.display()))?
                .collect::<std::result::Result<Vec<_>, _>>()
                .with_context(|| format!("reading directory entries for {}", start.display()))?;
            entries.sort_by_key(|entry| entry.file_name());
            paths.extend(
                entries
                    .into_iter()
                    .filter_map(|entry| entry.path().canonicalize().ok())
                    .filter(|path| jail.contains(path)),
            );
        }
        if paths.len() >= max_entries {
            break;
        }
    }
    paths.truncate(max_entries);

    let mut entries = Vec::new();
    for path in &paths {
        let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        entries.push(json!({
            "path": root.rel_path(path)?,
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
            "Read one text file, addressed as `<root>/<path>`. Read-only; cannot access paths outside the roots.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path as `<root>/<path>`." },
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
            "Read a line range from one text file, addressed as `<root>/<path>`. Lines are 1-based, inclusive.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path as `<root>/<path>`." },
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
    let requested_end = end_line.unwrap_or_else(|| start_line.saturating_add(max_lines - 1));
    let capped_end = requested_end.min(start_line.saturating_add(max_lines - 1));
    let reader = std::io::BufReader::new(fs::File::open(&path)?);
    let mut selected = Vec::new();
    let mut saw_more = false;
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        if line_no < start_line {
            continue;
        }
        if line_no > capped_end {
            saw_more = true;
            break;
        }
        selected.push(line?);
    }
    let line_truncated = requested_end > capped_end || saw_more;
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
        "total_lines": Value::Null,
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
            "Read multiple text files in one call, each addressed as `<root>/<path>`.",
            json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 50, "description": "File paths, each as `<root>/<path>`." },
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
            "Search text files for a case-insensitive substring across all roots. Returns snippets with `<root>/<path>` paths and line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring to search for." },
                    "path": { "type": "string", "description": "Optional `<root>` or `<root>/<path>` to scope the search. Omit to search every root." },
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

#[async_trait]
impl Capability for FsGrep {
    fn name(&self) -> &str {
        "fs_grep"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_grep",
            "Search text files with a Rust regular expression across all roots (or a `<root>`/`<root>/<path>` scope). Paths are reported as `<root>/<path>`.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"case_sensitive":{"type":"boolean"},"max_matches":{"type":"integer","minimum":1,"maximum":500}},"required":["pattern"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(grep_impl(&self.0, args))
    }
}

fn grep_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .context("fs_grep requires string `pattern`")?;
    let regex = RegexBuilder::new(pattern)
        .case_insensitive(
            !args
                .get("case_sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        )
        .build()
        .context("invalid fs_grep pattern")?;
    let limit = args
        .get("max_matches")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_MATCHES as u64)
        .clamp(1, 500) as usize;
    let files = collect_files(root, args.get("path").and_then(Value::as_str))?;
    let mut matches = Vec::new();
    let mut searched_files = 0;
    for file in files {
        let Ok(meta) = fs::metadata(&file) else {
            continue;
        };
        if !meta.is_file() || !looks_textual(&file) || meta.len() > DEFAULT_SEARCH_LIMIT_BYTES {
            continue;
        }
        searched_files += 1;
        let (content, _) = read_utf8_prefix(&file, DEFAULT_SEARCH_LIMIT_BYTES as usize)?;
        let rel = root.rel_path(&file)?;
        for (i, line) in content.lines().enumerate() {
            if regex.is_match(line) {
                matches.push(json!({"path":rel,"line":i+1,"snippet":line.trim()}));
                if matches.len() >= limit {
                    return Ok(
                        json!({"pattern":pattern,"matches":matches,"searched_files":searched_files,"truncated":true}),
                    );
                }
            }
        }
    }
    Ok(
        json!({"pattern":pattern,"matches":matches,"searched_files":searched_files,"truncated":false}),
    )
}

/// The text-search candidate files for an optional caller path: the file
/// itself, or every file under the resolved directory (or all roots when the
/// path is omitted).
fn collect_files(root: &FsRoot, path: Option<&str>) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for (jail, start) in root.walk_targets(path)? {
        if start.is_file() {
            files.push(start);
        } else {
            files.extend(walk_files(jail, &start, WALK_CEILING)?);
        }
    }
    Ok(files)
}

#[async_trait]
impl Capability for FsGlob {
    fn name(&self) -> &str {
        "fs_glob"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_glob",
            "Match file paths with a glob pattern; `**` crosses directories. Matches against the `<root>/<path>` form, so a pattern can target one root or span all.",
            json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"max_matches":{"type":"integer","minimum":1,"maximum":2000}},"required":["pattern"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        wrap(glob_impl(&self.0, args))
    }
}

fn glob_impl(root: &FsRoot, args: Value) -> Result<Value> {
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .context("fs_glob requires string `pattern`")?;
    let mut builder = GlobSetBuilder::new();
    builder.add(Glob::new(pattern).context("invalid fs_glob pattern")?);
    let matcher = builder.build()?;
    let limit = args
        .get("max_matches")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_LIST_LIMIT as u64)
        .clamp(1, 2000) as usize;
    let mut matches = Vec::new();
    for (jail, start) in root.walk_targets(args.get("path").and_then(Value::as_str))? {
        anyhow::ensure!(start.is_dir(), "fs_glob path must be a directory");
        for file in walk_files(jail, &start, WALK_CEILING)? {
            let rel = root.rel_path(&file)?;
            // In named mode the glob matches against the `<root>/<path>` form,
            // so a pattern can target one root or span all of them.
            if matcher.is_match(&rel) {
                matches.push(rel);
                if matches.len() >= limit {
                    return Ok(json!({"matches":matches,"truncated":true}));
                }
            }
        }
    }
    Ok(json!({"matches":matches,"truncated":false}))
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
    let files = collect_files(root, args.get("path").and_then(Value::as_str))?;
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
            "Return markdown-style headings for one text file or for text files under a directory. Paths are addressed as `<root>/<path>`; omit to span every root.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional file or directory as `<root>/<path>`. Omit to span every root." },
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
    let targets = root.walk_targets(args.get("path").and_then(Value::as_str))?;
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
    // A single named file is reported even when it has no headings.
    let start_is_file = matches!(targets.as_slice(), [(_, start)] if start.is_file());
    let candidates = if start_is_file {
        vec![targets[0].1.clone()]
    } else {
        let mut files = Vec::new();
        for (jail, start) in targets {
            if start.is_dir() {
                files.extend(walk_files(jail, &start, WALK_CEILING)?);
            }
        }
        files
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
                "huggr-fsread-{tag}-{}-{:p}",
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
        // A single root is still addressed by name (`r/...`).
        let fs_root = FsRoot::with_named(vec![("r".to_string(), dir.0.clone())]).unwrap();
        (dir, fs_root)
    }

    #[test]
    fn lists_reads_searches_greps_globs_and_outlines() {
        let (_dir, root) = root("basic");
        let listed = list_impl(&root, json!({ "path": "r", "recursive": true })).unwrap();
        let paths: Vec<_> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"r/a.md".to_string()));
        assert!(paths.contains(&"r/sub/b.txt".to_string()));

        let read = read_document(&root, "r/a.md", 1_000_000).unwrap();
        assert!(read["content"].as_str().unwrap().contains("hello world"));

        let searched = search_impl(&root, json!({ "query": "needle" })).unwrap();
        assert_eq!(searched["matches"].as_array().unwrap().len(), 1);
        assert_eq!(searched["matches"][0]["path"], "r/sub/b.txt");

        let grepped = grep_impl(
            &root,
            json!({ "pattern": "NEE.*", "case_sensitive": false }),
        )
        .unwrap();
        assert_eq!(grepped["matches"][0]["path"], "r/sub/b.txt");

        let globbed = glob_impl(&root, json!({ "pattern": "**/*.md" })).unwrap();
        assert_eq!(globbed["matches"][0], "r/a.md");

        let outline = outline_impl(&root, json!({ "path": "r/a.md" })).unwrap();
        assert_eq!(outline["documents"][0]["headings"][0]["text"], "Title");

        let ranged = read_range_document(&root, "r/a.md", 2, Some(2), 200, 1_000_000).unwrap();
        assert_eq!(ranged["content"], "hello world");
    }

    #[test]
    fn jail_rejects_traversal_and_absolute_paths() {
        let (_dir, root) = root("jail");
        assert!(root.resolve_existing(Some("r/../secret")).is_err());
        assert!(read_document(&root, "r/../a.md", 1000).is_err());
        // A legitimate in-jail path still resolves.
        assert!(root.resolve_existing(Some("r/sub/b.txt")).is_ok());
        // An unknown root name is rejected.
        assert!(root.resolve_existing(Some("nope/x")).is_err());
    }

    fn multi_root() -> (TmpDir, TmpDir, FsRoot) {
        let a = TmpDir::new("multi-a");
        a.write("main.md", "# A\nneedle in a\n");
        let b = TmpDir::new("multi-b");
        b.write("lib.md", "# B\nneedle in b\n");
        let root = FsRoot::with_named(vec![
            ("a".to_string(), a.0.clone()),
            ("b".to_string(), b.0.clone()),
        ])
        .unwrap();
        (a, b, root)
    }

    #[test]
    fn named_roots_address_files_by_name() {
        let (_a, _b, root) = multi_root();
        // No path lists the roots themselves as top-level dirs.
        let listed = list_impl(&root, json!({})).unwrap();
        let names: Vec<_> = listed["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&"a".to_string()) && names.contains(&"b".to_string()));

        // Reads are addressed as `<root>/<path>` and echo that form back.
        let read = read_document(&root, "a/main.md", 1_000_000).unwrap();
        assert_eq!(read["path"], "a/main.md");
        assert!(read["content"].as_str().unwrap().contains("needle in a"));

        // A bare path (no root name) and an unknown root both error.
        assert!(read_document(&root, "main.md", 100).is_err());
        assert!(read_document(&root, "c/main.md", 100).is_err());
    }

    #[test]
    fn named_roots_search_across_all_and_scope_to_one() {
        let (_a, _b, root) = multi_root();
        let all = search_impl(&root, json!({ "query": "needle" })).unwrap();
        let paths: Vec<_> = all["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert!(paths.contains(&"a/main.md".to_string()));
        assert!(paths.contains(&"b/lib.md".to_string()));

        let scoped = search_impl(&root, json!({ "query": "needle", "path": "b" })).unwrap();
        let scoped: Vec<_> = scoped["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(scoped, vec!["b/lib.md".to_string()]);

        let globbed = glob_impl(&root, json!({ "pattern": "**/*.md" })).unwrap();
        let globs: Vec<_> = globbed["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap().to_string())
            .collect();
        assert!(
            globs.contains(&"a/main.md".to_string()) && globs.contains(&"b/lib.md".to_string())
        );
    }

    #[test]
    fn named_roots_stay_isolated_from_each_other() {
        let (_a, _b, root) = multi_root();
        // A traversal out of one root cannot reach a sibling root.
        assert!(root.resolve_existing(Some("a/../b/lib.md")).is_err());
        // Duplicate root names are rejected at construction.
        assert!(
            FsRoot::with_named(vec![
                ("dup".to_string(), _a.0.clone()),
                ("dup".to_string(), _b.0.clone()),
            ])
            .is_err()
        );
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
            "huggr-fsread-secret-{}-{:p}",
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
        let err = root.resolve_existing(Some("r/escape.md")).unwrap_err();
        assert!(err.to_string().contains("escapes the tool root"), "{err}");
        assert!(read_document(&root, "r/escape.md", 1000).is_err());

        let _ = fs::remove_file(&outside);
    }
}
