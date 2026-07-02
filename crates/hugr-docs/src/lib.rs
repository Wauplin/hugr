use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use hugr_core::{
    Decision, DoneReason, ModelSelector, OpId, OpMeta, OpOutcome, OutputEvent, Record,
    SamplingParams, ToolSchema, Usage, Value,
};
use hugr_host::{Capability, ChunkSink, Frontend, estimate_text_tokens};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const DEFAULT_MODEL: &str = "google/gemma-4-31B-it:cerebras";
pub const DEFAULT_BASE_URL: &str = "https://router.huggingface.co/v1";
pub const DEFAULT_INPUT_USD_PER_M_TOKENS: f64 = 1.0;
pub const DEFAULT_OUTPUT_USD_PER_M_TOKENS: f64 = 1.5;

const DEFAULT_READ_LIMIT_BYTES: usize = 200_000;
const MAX_READ_LIMIT_BYTES: usize = 1_000_000;
const DEFAULT_SEARCH_LIMIT_BYTES: u64 = 512_000;
const DEFAULT_MAX_MATCHES: usize = 50;
const DEFAULT_LIST_LIMIT: usize = 500;
const DEFAULT_RANGE_MAX_LINES: usize = 200;
const MAX_RANGE_LINES: usize = 5_000;
const MAX_BATCH_READS: usize = 50;
const DEFAULT_OUTLINE_MAX_DOCUMENTS: usize = 100;
const DEFAULT_OUTLINE_MAX_HEADINGS: usize = 1_000;

pub const SYSTEM_PROMPT: &str = "\
You are a documentation retrieval agent. Answer the user's question using only the documentation available through the provided read-only tools. Start by searching or listing the docs, then read every source document needed to support the answer. AI_INDEX.md files are navigation aids only: use them to decide what to read, but never cite them as related documents. If the docs do not contain enough evidence, say that it is not possible to find an answer in the docs. Do not use prior knowledge. Your final response must be a single JSON object with exactly these fields: answer (string) and related_documents (array of document paths relative to the docs root, excluding AI_INDEX.md).";

#[derive(Clone, Debug)]
pub struct DocsConfig {
    pub root: PathBuf,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub input_usd_per_m_tokens: f64,
    pub output_usd_per_m_tokens: f64,
    pub sampling: SamplingParams,
}

impl DocsConfig {
    pub fn from_env(root: PathBuf, model_override: Option<String>) -> Result<Self> {
        let api_key = std::env::var("HUGR_DOCS_API_KEY").context("set HUGR_DOCS_API_KEY")?;
        let model = model_override
            .or_else(|| std::env::var("HUGR_DOCS_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url =
            std::env::var("HUGR_DOCS_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let input_usd_per_m_tokens = parse_env_f64(
            "HUGR_DOCS_INPUT_USD_PER_M_TOKENS",
            DEFAULT_INPUT_USD_PER_M_TOKENS,
        )?;
        let output_usd_per_m_tokens = parse_env_f64(
            "HUGR_DOCS_OUTPUT_USD_PER_M_TOKENS",
            DEFAULT_OUTPUT_USD_PER_M_TOKENS,
        )?;
        let sampling = SamplingParams::new().with_temperature(0.0);
        Ok(Self {
            root,
            model,
            base_url,
            api_key,
            input_usd_per_m_tokens,
            output_usd_per_m_tokens,
            sampling,
        })
    }
}

fn parse_env_f64(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(value) => value
            .parse::<f64>()
            .with_context(|| format!("parsing {name}={value:?}")),
        Err(_) => Ok(default),
    }
}

#[derive(Clone, Debug)]
pub struct DocsRoot {
    root: Arc<PathBuf>,
}

impl DocsRoot {
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root
            .as_ref()
            .canonicalize()
            .with_context(|| format!("canonicalizing docs root {}", root.as_ref().display()))?;
        anyhow::ensure!(
            root.is_dir(),
            "docs root is not a directory: {}",
            root.display()
        );
        Ok(Self {
            root: Arc::new(root),
        })
    }

    pub fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        vec![
            Arc::new(DocsList::new(self.clone())),
            Arc::new(DocsSearch::new(self.clone())),
            Arc::new(DocsRead::new(self.clone())),
            Arc::new(DocsReadRange::new(self.clone())),
            Arc::new(DocsReadMany::new(self.clone())),
            Arc::new(DocsReadRangeMany::new(self.clone())),
            Arc::new(DocsOutline::new(self.clone())),
        ]
    }

    fn root(&self) -> &Path {
        self.root.as_path()
    }

    fn resolve_existing(&self, rel: Option<&str>) -> Result<PathBuf> {
        let rel = rel.unwrap_or("").trim();
        let candidate = if rel.is_empty() {
            self.root().to_path_buf()
        } else {
            let path = Path::new(rel);
            anyhow::ensure!(!path.is_absolute(), "path must be relative to docs root");
            for component in path.components() {
                match component {
                    Component::Normal(_) | Component::CurDir => {}
                    Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                        return Err(anyhow!("path escapes docs root"));
                    }
                }
            }
            self.root().join(path)
        };
        let canonical = candidate
            .canonicalize()
            .with_context(|| format!("path does not exist inside docs root: {rel}"))?;
        anyhow::ensure!(
            canonical.starts_with(self.root()),
            "path escapes docs root: {rel}"
        );
        Ok(canonical)
    }

    fn rel_path(&self, path: &Path) -> Result<String> {
        let rel = path.strip_prefix(self.root()).with_context(|| {
            format!(
                "path {} is not under docs root {}",
                path.display(),
                self.root().display()
            )
        })?;
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

fn is_ai_index(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == "AI_INDEX.md")
}

fn looks_textual(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "md" | "mdx" | "txt" | "rst" | "adoc" | "json" | "yaml" | "yml" | "toml"
    )
}

fn read_utf8_prefix(path: &Path, limit: usize) -> Result<(String, bool)> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let truncated = bytes.len() > limit;
    let slice = if truncated { &bytes[..limit] } else { &bytes };
    Ok((String::from_utf8_lossy(slice).into_owned(), truncated))
}

fn walk_files(root: &DocsRoot, start: &Path, limit: usize) -> Result<Vec<PathBuf>> {
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

#[derive(Clone)]
struct DocsList {
    root: DocsRoot,
}

impl DocsList {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsList {
    fn name(&self) -> &str {
        "docs_list"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "List files and directories under the docs root. Paths are relative to the docs root.",
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
        match list_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn list_impl(root: &DocsRoot, args: Value) -> Result<Value> {
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
    anyhow::ensure!(start.is_dir(), "docs_list path must be a directory");

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
            "is_index": root.rel_path(&path).is_ok_and(|p| is_ai_index(&p)),
        }));
    }
    Ok(json!({ "entries": entries, "truncated": entries.len() >= max_entries }))
}

#[derive(Clone)]
struct DocsRead {
    root: DocsRoot,
}

impl DocsRead {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsRead {
    fn name(&self) -> &str {
        "docs_read"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read one text document under the docs root. This is read-only and cannot access paths outside the docs root.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Document path relative to the docs root." },
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
        match read_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn read_impl(root: &DocsRoot, args: Value) -> Result<Value> {
    let rel = args
        .get("path")
        .and_then(Value::as_str)
        .context("docs_read requires string `path`")?;
    let limit = args
        .get("max_bytes")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_LIMIT_BYTES as u64)
        .clamp(1, MAX_READ_LIMIT_BYTES as u64) as usize;
    read_document(root, rel, limit)
}

fn read_document(root: &DocsRoot, rel: &str, limit: usize) -> Result<Value> {
    let path = root.resolve_existing(Some(rel))?;
    anyhow::ensure!(path.is_file(), "docs_read path must be a file");
    let rel = root.rel_path(&path)?;
    let (content, truncated) = read_utf8_prefix(&path, limit)?;
    Ok(json!({
        "path": rel,
        "is_index": is_ai_index(&rel),
        "bytes_returned": content.len(),
        "truncated": truncated,
        "content": content,
    }))
}

#[derive(Clone)]
struct DocsReadRange {
    root: DocsRoot,
}

impl DocsReadRange {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsReadRange {
    fn name(&self) -> &str {
        "docs_read_range"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read a line range from one text document under the docs root. Line numbers are 1-based and inclusive.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Document path relative to the docs root." },
                    "start_line": { "type": "integer", "minimum": 1, "description": "First line to read, 1-based." },
                    "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read, inclusive. If omitted, max_lines controls the window." },
                    "max_lines": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum lines to return when end_line is omitted or too large. Defaults to 200." },
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
        match read_range_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn read_range_impl(root: &DocsRoot, args: Value) -> Result<Value> {
    let rel = args
        .get("path")
        .and_then(Value::as_str)
        .context("docs_read_range requires string `path`")?;
    let start_line = args
        .get("start_line")
        .and_then(Value::as_u64)
        .context("docs_read_range requires integer `start_line`")? as usize;
    let end_line = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|line| line as usize);
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
    read_range_document(root, rel, start_line, end_line, max_lines, max_bytes)
}

fn read_range_document(
    root: &DocsRoot,
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
    anyhow::ensure!(path.is_file(), "docs_read_range path must be a file");
    let rel = root.rel_path(&path)?;
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let content = String::from_utf8_lossy(&bytes);
    let lines = content.lines().collect::<Vec<_>>();
    let requested_end = end_line.unwrap_or_else(|| start_line.saturating_add(max_lines - 1));
    let capped_end = requested_end.min(start_line.saturating_add(max_lines - 1));
    let selected = if start_line > lines.len() {
        Vec::new()
    } else {
        lines[(start_line - 1)..lines.len().min(capped_end)]
            .iter()
            .copied()
            .collect::<Vec<_>>()
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
        "is_index": is_ai_index(&rel),
        "start_line": start_line,
        "end_line": end_line_returned,
        "total_lines": lines.len(),
        "bytes_returned": content.len(),
        "truncated": line_truncated || byte_truncated,
        "content": content,
    }))
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

#[derive(Clone)]
struct DocsReadMany {
    root: DocsRoot,
}

impl DocsReadMany {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsReadMany {
    fn name(&self) -> &str {
        "docs_read_many"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read multiple text documents under the docs root in one call.",
            json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "minItems": 1, "maxItems": 50, "description": "Document paths relative to the docs root." },
                    "max_bytes_per_document": { "type": "integer", "minimum": 1, "maximum": 1000000, "description": "Maximum bytes to return per document. Defaults to 200000." },
                    "max_documents": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum documents to read from paths. Defaults to 50." }
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
        match read_many_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn read_many_impl(root: &DocsRoot, args: Value) -> Result<Value> {
    let paths = args
        .get("paths")
        .and_then(Value::as_array)
        .context("docs_read_many requires array `paths`")?;
    anyhow::ensure!(!paths.is_empty(), "docs_read_many paths cannot be empty");
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

#[derive(Clone)]
struct DocsReadRangeMany {
    root: DocsRoot,
}

impl DocsReadRangeMany {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsReadRangeMany {
    fn name(&self) -> &str {
        "docs_read_range_many"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read line ranges from multiple text documents under the docs root in one call.",
            json!({
                "type": "object",
                "properties": {
                    "ranges": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 50,
                        "items": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Document path relative to the docs root." },
                                "start_line": { "type": "integer", "minimum": 1, "description": "First line to read, 1-based." },
                                "end_line": { "type": "integer", "minimum": 1, "description": "Last line to read, inclusive." },
                                "max_lines": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum lines to return for this range. Defaults to 200." }
                            },
                            "required": ["path", "start_line"],
                            "additionalProperties": false
                        },
                        "description": "Line ranges to read."
                    },
                    "max_bytes_per_range": { "type": "integer", "minimum": 1, "maximum": 1000000, "description": "Maximum bytes to return per range. Defaults to 200000." },
                    "max_ranges": { "type": "integer", "minimum": 1, "maximum": 50, "description": "Maximum ranges to read. Defaults to 50." }
                },
                "required": ["ranges"],
                "additionalProperties": false
            }),
        )
    }

    fn requires_permission(&self) -> bool {
        false
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        match read_range_many_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn read_range_many_impl(root: &DocsRoot, args: Value) -> Result<Value> {
    let ranges = args
        .get("ranges")
        .and_then(Value::as_array)
        .context("docs_read_range_many requires array `ranges`")?;
    anyhow::ensure!(
        !ranges.is_empty(),
        "docs_read_range_many ranges cannot be empty"
    );
    let max_ranges = args
        .get("max_ranges")
        .and_then(Value::as_u64)
        .unwrap_or(MAX_BATCH_READS as u64)
        .clamp(1, MAX_BATCH_READS as u64) as usize;
    let max_bytes = args
        .get("max_bytes_per_range")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_READ_LIMIT_BYTES as u64)
        .clamp(1, MAX_READ_LIMIT_BYTES as u64) as usize;
    let mut documents = Vec::new();
    let mut errors = Vec::new();
    for range in ranges.iter().take(max_ranges) {
        let Some(object) = range.as_object() else {
            errors.push(json!({ "range": range, "error": "range must be an object" }));
            continue;
        };
        let Some(rel) = object.get("path").and_then(Value::as_str) else {
            errors.push(json!({ "range": range, "error": "range path must be a string" }));
            continue;
        };
        let Some(start_line) = object.get("start_line").and_then(Value::as_u64) else {
            errors.push(json!({ "path": rel, "error": "start_line must be an integer" }));
            continue;
        };
        let end_line = object
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|line| line as usize);
        let max_lines = object
            .get("max_lines")
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_RANGE_MAX_LINES as u64)
            .clamp(1, MAX_RANGE_LINES as u64) as usize;
        match read_range_document(
            root,
            rel,
            start_line as usize,
            end_line,
            max_lines,
            max_bytes,
        ) {
            Ok(document) => documents.push(document),
            Err(error) => errors.push(json!({ "path": rel, "error": error.to_string() })),
        }
    }
    Ok(json!({
        "documents": documents,
        "errors": errors,
        "truncated": ranges.len() > max_ranges,
    }))
}

#[derive(Clone)]
struct DocsOutline {
    root: DocsRoot,
}

impl DocsOutline {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsOutline {
    fn name(&self) -> &str {
        "docs_outline"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Return markdown-style headings for one text document or for text documents under a directory.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Optional relative file or directory path. Defaults to the docs root." },
                    "max_documents": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum text documents to inspect. Defaults to 100." },
                    "max_headings": { "type": "integer", "minimum": 1, "maximum": 5000, "description": "Maximum headings to return across all inspected documents. Defaults to 1000." }
                },
                "additionalProperties": false
            }),
        )
    }

    fn requires_permission(&self) -> bool {
        false
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        match outline_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn outline_impl(root: &DocsRoot, args: Value) -> Result<Value> {
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
        walk_files(root, &start, 20_000)?
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
        let (content, content_truncated) =
            read_utf8_prefix(&file, DEFAULT_SEARCH_LIMIT_BYTES as usize)?;
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
            headings.push(json!({
                "line": line_idx + 1,
                "level": level,
                "text": text,
            }));
        }
        if start_is_file || !headings.is_empty() {
            documents.push(json!({
                "path": rel,
                "is_index": is_ai_index(&rel),
                "headings": headings,
                "truncated": content_truncated || hit_heading_limit,
            }));
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

#[derive(Clone)]
struct DocsSearch {
    root: DocsRoot,
}

impl DocsSearch {
    fn new(root: DocsRoot) -> Self {
        Self { root }
    }
}

#[async_trait]
impl Capability for DocsSearch {
    fn name(&self) -> &str {
        "docs_search"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Search text documents under the docs root for a case-insensitive substring. Returns snippets with relative paths and line numbers.",
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
        match search_impl(&self.root, args) {
            Ok(value) => Ok(value),
            Err(error) => Err(json!({ "error": error.to_string() })),
        }
    }
}

fn search_impl(root: &DocsRoot, args: Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(Value::as_str)
        .context("docs_search requires string `query`")?
        .trim();
    anyhow::ensure!(!query.is_empty(), "docs_search query cannot be empty");
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
        walk_files(root, &start, 20_000)?
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
                    "is_index": is_ai_index(&rel),
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

#[derive(Default)]
pub struct JsonFrontend;

impl Frontend for JsonFrontend {
    fn on_output(&mut self, event: &OutputEvent) {
        match event {
            OutputEvent::ModelText { op, text } => {
                eprintln!(
                    "[hugr-docs] model_text op={} chunk={}",
                    op.0,
                    one_line_json(text)
                );
            }
            OutputEvent::ModelReasoning { op, text } => {
                eprintln!(
                    "[hugr-docs] model_reasoning op={} chunk={}",
                    op.0,
                    one_line_json(text)
                );
            }
            OutputEvent::ToolCallStarted { op, id, name } => {
                eprintln!(
                    "[hugr-docs] tool_call_started op={} id={} name={}",
                    op.0, id, name
                );
            }
            OutputEvent::ToolChunk { op, chunk } => {
                eprintln!(
                    "[hugr-docs] tool_chunk op={} chunk={}",
                    op.0,
                    summarize_value(chunk)
                );
            }
            OutputEvent::Notice(message) => {
                eprintln!("[hugr-docs] notice {}", message);
            }
            _ => {
                eprintln!("[hugr-docs] output {:?}", event);
            }
        }
    }

    fn on_notice(&mut self, message: &str) {
        eprintln!("[hugr-docs] notice {message}");
    }

    fn on_model_start(&mut self, op: OpId, selector: &ModelSelector) {
        eprintln!(
            "[hugr-docs] model_start op={} selector={:?}",
            op.0, selector
        );
    }

    fn on_model_end(&mut self, op: OpId, usage: &Usage) {
        eprintln!(
            "[hugr-docs] model_end op={} input_tokens={} output_tokens={}",
            op.0, usage.input_tokens, usage.output_tokens
        );
    }

    fn on_tool_start(&mut self, op: OpId, name: &str, args: &Value) {
        eprintln!(
            "[hugr-docs] tool_start op={} name={} args={}",
            op.0,
            name,
            summarize_value(args)
        );
    }

    fn on_tool_end(&mut self, op: OpId, name: &str, result: &Value, is_error: bool) {
        let status = if is_error { "error" } else { "ok" };
        eprintln!(
            "[hugr-docs] tool_end op={} name={} status={} result={}",
            op.0,
            name,
            status,
            summarize_tool_result(name, result)
        );
    }

    fn on_permission(&mut self, capability: &str, decision: &Decision) {
        eprintln!(
            "[hugr-docs] permission capability={} decision={:?}",
            capability, decision
        );
    }

    fn on_done(&mut self, reason: &DoneReason) {
        eprintln!("[hugr-docs] done reason={:?}", reason);
    }

    fn on_session_end(&mut self) {
        eprintln!("[hugr-docs] session_end");
    }
}

fn summarize_tool_result(name: &str, result: &Value) -> String {
    if let Some(error) = result.get("error") {
        return format!("error={}", summarize_value(error));
    }
    match name {
        "docs_read" => format!(
            "path={} bytes_returned={} truncated={} is_index={}",
            display_json_field(result, "path"),
            display_json_field(result, "bytes_returned"),
            display_json_field(result, "truncated"),
            display_json_field(result, "is_index")
        ),
        "docs_read_range" => format!(
            "path={} start_line={} end_line={} bytes_returned={} truncated={} is_index={}",
            display_json_field(result, "path"),
            display_json_field(result, "start_line"),
            display_json_field(result, "end_line"),
            display_json_field(result, "bytes_returned"),
            display_json_field(result, "truncated"),
            display_json_field(result, "is_index")
        ),
        "docs_read_many" | "docs_read_range_many" => format!(
            "documents={} errors={} truncated={}",
            result
                .get("documents")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            result
                .get("errors")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            display_json_field(result, "truncated")
        ),
        "docs_outline" => format!(
            "documents={} searched_files={} truncated={}",
            result
                .get("documents")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            display_json_field(result, "searched_files"),
            display_json_field(result, "truncated")
        ),
        "docs_search" => format!(
            "query={} matches={} searched_files={} truncated={}",
            display_json_field(result, "query"),
            result
                .get("matches")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            display_json_field(result, "searched_files"),
            display_json_field(result, "truncated")
        ),
        "docs_list" => format!(
            "entries={} truncated={}",
            result
                .get("entries")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            display_json_field(result, "truncated")
        ),
        _ => summarize_value(result),
    }
}

fn display_json_field(value: &Value, field: &str) -> String {
    value
        .get(field)
        .map(summarize_value)
        .unwrap_or_else(|| "null".to_string())
}

fn summarize_value(value: &Value) -> String {
    match value {
        Value::String(text) => one_line_json(text),
        Value::Array(items) => format!("[{} item(s)]", items.len()),
        Value::Object(map) => {
            let keys = map.keys().cloned().collect::<Vec<_>>().join(",");
            format!("{{keys={keys}}}")
        }
        other => other.to_string(),
    }
}

fn one_line_json(text: &str) -> String {
    serde_json::to_string(text).unwrap_or_else(|_| format!("{text:?}"))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnswerPayload {
    pub answer: String,
    #[serde(default)]
    pub related_documents: Vec<String>,
}

impl AnswerPayload {
    pub fn from_model_text(text: &str) -> Self {
        let trimmed = text.trim();
        let candidate = strip_json_fence(trimmed);
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            let answer = value
                .get("answer")
                .and_then(Value::as_str)
                .unwrap_or(trimmed)
                .trim()
                .to_string();
            let related_documents = value
                .get("related_documents")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            return Self {
                answer,
                related_documents,
            };
        }
        Self {
            answer: trimmed.to_string(),
            related_documents: Vec::new(),
        }
    }
}

fn strip_json_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest
        .strip_prefix("json")
        .or_else(|| rest.strip_prefix("JSON"))
        .unwrap_or(rest)
        .trim_start_matches(['\r', '\n']);
    rest.strip_suffix("```").unwrap_or(rest).trim()
}

#[derive(Clone, Debug, Serialize)]
pub struct DocsAnswer {
    pub answer: String,
    pub related_documents: Vec<String>,
    pub metadata: RunMetadata,
}

#[derive(Clone, Debug, Serialize)]
pub struct RunMetadata {
    pub model: String,
    pub endpoint: String,
    pub elapsed_ms: u128,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub estimated_cost_micro_usd: u64,
    pub input_usd_per_m_tokens: f64,
    pub output_usd_per_m_tokens: f64,
    pub model_calls: usize,
    pub tool_calls: usize,
    pub read_documents: usize,
    pub read_indexes: usize,
}

pub fn build_answer(
    log: &[hugr_core::LogEntry],
    config: &DocsConfig,
    elapsed: Duration,
) -> Result<DocsAnswer> {
    let final_text = log
        .iter()
        .rev()
        .find_map(|entry| match &entry.record {
            Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
                Some(output.text.as_str())
            }
            _ => None,
        })
        .ok_or_else(|| missing_final_answer_error(log))?;
    let payload = AnswerPayload::from_model_text(final_text);
    let read = read_document_sets(log);
    let related_documents = sanitize_related_documents(payload.related_documents, &read.documents);
    let (tokens_in, tokens_out, model_calls, tool_calls) = usage_totals(log);
    let estimated_cost_micro_usd = estimate_cost_micro_usd(
        tokens_in,
        tokens_out,
        config.input_usd_per_m_tokens,
        config.output_usd_per_m_tokens,
    );
    Ok(DocsAnswer {
        answer: payload.answer,
        related_documents,
        metadata: RunMetadata {
            model: config.model.clone(),
            endpoint: config.base_url.clone(),
            elapsed_ms: elapsed.as_millis(),
            tokens_in,
            tokens_out,
            estimated_cost_micro_usd,
            input_usd_per_m_tokens: config.input_usd_per_m_tokens,
            output_usd_per_m_tokens: config.output_usd_per_m_tokens,
            model_calls,
            tool_calls,
            read_documents: read.documents.len(),
            read_indexes: read.indexes.len(),
        },
    })
}

fn missing_final_answer_error(log: &[hugr_core::LogEntry]) -> anyhow::Error {
    if let Some(message) = last_terminal_error(log) {
        return anyhow!("model did not produce a final answer; {message}");
    }
    let model_outputs = log
        .iter()
        .filter(|entry| matches!(entry.record, Record::ModelOutput { .. }))
        .count();
    let tool_results = log
        .iter()
        .filter(|entry| matches!(entry.record, Record::ToolResult { .. }))
        .count();
    anyhow!(
        "model did not produce a final answer; log contains {model_outputs} model output(s) and {tool_results} tool result(s)"
    )
}

fn last_terminal_error(log: &[hugr_core::LogEntry]) -> Option<String> {
    log.iter().rev().find_map(|entry| {
        let Record::OpEnded { op, outcome, meta } = &entry.record else {
            return None;
        };
        match outcome {
            OpOutcome::Error(error) => Some(format!(
                "{} operation {} failed: {}",
                op_kind(meta),
                op.0,
                value_message(error)
            )),
            OpOutcome::Cancelled { partial } => Some(format!(
                "{} operation {} was cancelled with partial output: {}",
                op_kind(meta),
                op.0,
                value_message(partial)
            )),
            OpOutcome::Ok => None,
            _ => None,
        }
    })
}

fn op_kind(meta: &OpMeta) -> &'static str {
    if meta.model.is_some() {
        "model"
    } else {
        "tool"
    }
}

fn value_message(value: &Value) -> String {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return message.to_string();
    }
    if let Some(error) = value.get("error") {
        if let Some(message) = error.as_str() {
            return message.to_string();
        }
        return error.to_string();
    }
    value.to_string()
}

fn estimate_cost_micro_usd(
    tokens_in: u64,
    tokens_out: u64,
    in_usd_per_m: f64,
    out_usd_per_m: f64,
) -> u64 {
    ((tokens_in as f64 * in_usd_per_m) + (tokens_out as f64 * out_usd_per_m)).round() as u64
}

#[derive(Default)]
struct ReadSets {
    documents: BTreeSet<String>,
    indexes: BTreeSet<String>,
}

fn read_document_sets(log: &[hugr_core::LogEntry]) -> ReadSets {
    let mut read = ReadSets::default();
    for entry in log {
        let Record::ToolResult { name, result, .. } = &entry.record else {
            continue;
        };
        match name.as_str() {
            "docs_read" | "docs_read_range" => {
                collect_read_path(&mut read, result);
            }
            "docs_read_many" | "docs_read_range_many" | "docs_outline" => {
                if let Some(documents) = result.get("documents").and_then(Value::as_array) {
                    for document in documents {
                        collect_read_path(&mut read, document);
                    }
                }
            }
            _ => {}
        }
    }
    read
}

fn collect_read_path(read: &mut ReadSets, value: &Value) {
    let Some(path) = value.get("path").and_then(Value::as_str) else {
        return;
    };
    if value
        .get("is_index")
        .and_then(Value::as_bool)
        .unwrap_or_else(|| is_ai_index(path))
    {
        read.indexes.insert(path.to_string());
    } else {
        read.documents.insert(path.to_string());
    }
}

fn sanitize_related_documents(
    model_related: Vec<String>,
    fallback_read_docs: &BTreeSet<String>,
) -> Vec<String> {
    let mut out = BTreeSet::new();
    for path in model_related {
        let path = path.trim().trim_start_matches("./");
        if path.is_empty()
            || is_ai_index(path)
            || Path::new(path).is_absolute()
            || !fallback_read_docs.contains(path)
        {
            continue;
        }
        if Path::new(path)
            .components()
            .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
        {
            continue;
        }
        out.insert(path.to_string());
    }
    if out.is_empty() {
        out.extend(fallback_read_docs.iter().cloned());
    }
    out.into_iter().collect()
}

fn usage_totals(log: &[hugr_core::LogEntry]) -> (u64, u64, usize, usize) {
    let mut tokens_in = 0;
    let mut tokens_out = 0;
    let mut model_calls = 0;
    let mut tool_calls = 0;
    for entry in log {
        let Record::OpEnded { meta, .. } = &entry.record else {
            continue;
        };
        if let Some(usage) = &meta.usage {
            tokens_in += usage.input_tokens;
            tokens_out += usage.output_tokens;
            model_calls += 1;
        } else if meta.model.is_none() {
            tool_calls += 1;
        }
    }
    (tokens_in, tokens_out, model_calls, tool_calls)
}

pub fn user_prompt(question: &str) -> String {
    format!(
        "Question: {question}\n\nReturn only the final JSON object after using the docs tools. If the answer is absent from the docs, use this answer exactly: \"It is not possible to find an answer in the docs.\""
    )
}

pub fn prompt_est_tokens(question: &str) -> u32 {
    estimate_text_tokens(&user_prompt(question))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_core::{LogEntry, OpId, Seq, Timestamp};

    #[test]
    fn parses_json_from_fenced_model_text() {
        let payload = AnswerPayload::from_model_text(
            "```json\n{\"answer\":\"A\",\"related_documents\":[\"hub/notifications.md\"]}\n```",
        );
        assert_eq!(payload.answer, "A");
        assert_eq!(payload.related_documents, vec!["hub/notifications.md"]);
    }

    #[test]
    fn filters_related_documents_and_falls_back_to_reads() {
        let fallback = BTreeSet::from([
            "guide/start.md".to_string(),
            "hub/notifications.md".to_string(),
        ]);
        let docs = sanitize_related_documents(
            vec![
                "AI_INDEX.md".to_string(),
                "../secret.md".to_string(),
                "/tmp/nope.md".to_string(),
                "./hub/notifications.md".to_string(),
            ],
            &fallback,
        );
        assert_eq!(docs, vec!["hub/notifications.md"]);

        let docs = sanitize_related_documents(vec!["unread-but-valid.md".to_string()], &fallback);
        assert_eq!(docs, vec!["guide/start.md", "hub/notifications.md"]);

        let docs = sanitize_related_documents(vec!["AI_INDEX.md".to_string()], &fallback);
        assert_eq!(docs, vec!["guide/start.md", "hub/notifications.md"]);
    }

    #[test]
    fn missing_final_answer_reports_terminal_error() {
        let log = vec![LogEntry {
            seq: Seq(0),
            at: Timestamp(0),
            record: serde_json::from_value(json!({
                "OpEnded": {
                    "op": 7,
                    "outcome": { "Error": { "message": "provider rejected tools" } },
                    "meta": {
                        "started_at": 0,
                        "ended_at": 1,
                        "model": { "Named": "docs" },
                        "usage": null,
                        "extra": null
                    }
                }
            }))
            .unwrap(),
        }];
        let message = missing_final_answer_error(&log).to_string();
        assert!(message.contains("model operation 7 failed: provider rejected tools"));
    }

    #[test]
    fn read_range_returns_line_window() {
        let dir = test_docs_dir();
        fs::write(
            dir.join("docs/page.md"),
            "one\ntwo\nthree\nfour\nfive\nsix\n",
        )
        .unwrap();

        let root = DocsRoot::new(dir.join("docs")).unwrap();
        let value = read_range_impl(
            &root,
            json!({ "path": "page.md", "start_line": 2, "end_line": 4 }),
        )
        .unwrap();
        assert_eq!(value["content"], "two\nthree\nfour");
        assert_eq!(value["start_line"], 2);
        assert_eq!(value["end_line"], 4);
        assert_eq!(value["truncated"], false);

        let value = read_range_impl(
            &root,
            json!({ "path": "page.md", "start_line": 3, "max_lines": 2 }),
        )
        .unwrap();
        assert_eq!(value["content"], "three\nfour");
        assert_eq!(value["truncated"], true);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_many_returns_partial_successes_and_errors() {
        let dir = test_docs_dir();
        fs::write(dir.join("docs/a.md"), "alpha").unwrap();
        fs::write(dir.join("docs/b.md"), "bravo").unwrap();
        fs::write(dir.join("secret.md"), "nope").unwrap();

        let root = DocsRoot::new(dir.join("docs")).unwrap();
        let value =
            read_many_impl(&root, json!({ "paths": ["a.md", "../secret.md", "b.md"] })).unwrap();
        let documents = value["documents"].as_array().unwrap();
        let errors = value["errors"].as_array().unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0]["path"], "a.md");
        assert_eq!(documents[1]["path"], "b.md");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["path"], "../secret.md");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_range_many_returns_line_windows() {
        let dir = test_docs_dir();
        fs::write(dir.join("docs/a.md"), "a1\na2\na3\n").unwrap();
        fs::write(dir.join("docs/b.md"), "b1\nb2\nb3\n").unwrap();

        let root = DocsRoot::new(dir.join("docs")).unwrap();
        let value = read_range_many_impl(
            &root,
            json!({
                "ranges": [
                    { "path": "a.md", "start_line": 2, "end_line": 3 },
                    { "path": "b.md", "start_line": 1, "max_lines": 1 }
                ]
            }),
        )
        .unwrap();
        let documents = value["documents"].as_array().unwrap();
        assert_eq!(documents.len(), 2);
        assert_eq!(documents[0]["content"], "a2\na3");
        assert_eq!(documents[1]["content"], "b1");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn outline_extracts_markdown_headings() {
        let dir = test_docs_dir();
        fs::write(
            dir.join("docs/page.md"),
            "# Title\n\nbody\n## Child\n### Deep ###\nnot # heading\n",
        )
        .unwrap();

        let root = DocsRoot::new(dir.join("docs")).unwrap();
        let value = outline_impl(&root, json!({ "path": "page.md" })).unwrap();
        let documents = value["documents"].as_array().unwrap();
        let headings = documents[0]["headings"].as_array().unwrap();
        assert_eq!(headings.len(), 3);
        assert_eq!(headings[0]["line"], 1);
        assert_eq!(headings[0]["level"], 1);
        assert_eq!(headings[0]["text"], "Title");
        assert_eq!(headings[2]["text"], "Deep");

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn read_document_sets_counts_new_read_tools() {
        let log = vec![
            tool_result(
                0,
                "docs_read_range",
                json!({ "path": "guide/a.md", "is_index": false }),
            ),
            tool_result(
                1,
                "docs_read_many",
                json!({
                    "documents": [
                        { "path": "guide/b.md", "is_index": false },
                        { "path": "AI_INDEX.md", "is_index": true }
                    ]
                }),
            ),
            tool_result(
                2,
                "docs_outline",
                json!({
                    "documents": [
                        { "path": "guide/c.md", "is_index": false, "headings": [] }
                    ]
                }),
            ),
        ];
        let read = read_document_sets(&log);
        assert_eq!(
            read.documents,
            BTreeSet::from([
                "guide/a.md".to_string(),
                "guide/b.md".to_string(),
                "guide/c.md".to_string()
            ])
        );
        assert_eq!(read.indexes, BTreeSet::from(["AI_INDEX.md".to_string()]));
    }

    #[tokio::test]
    async fn read_tool_cannot_escape_root() {
        let dir = test_docs_dir();
        fs::write(dir.join("docs/page.md"), "hello").unwrap();
        fs::write(dir.join("secret.md"), "nope").unwrap();

        let root = DocsRoot::new(dir.join("docs")).unwrap();
        let ok = read_impl(&root, json!({ "path": "page.md" })).unwrap();
        assert_eq!(ok["content"], "hello");
        assert!(read_impl(&root, json!({ "path": "../secret.md" })).is_err());

        let _ = fs::remove_dir_all(dir);
    }

    fn test_docs_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hugr-docs-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(dir.join("docs")).unwrap();
        dir
    }

    fn tool_result(seq: u64, name: &str, result: Value) -> LogEntry {
        LogEntry {
            seq: Seq(seq),
            at: Timestamp(seq),
            record: Record::ToolResult {
                op: OpId(seq),
                name: name.to_string(),
                call_id: format!("call-{seq}"),
                result,
                version: None,
                est_tokens: 0,
            },
        }
    }
}
