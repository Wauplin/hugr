use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use hugr_agent::{Ask, TraceId};
use hugr_core::{OpMeta, OpOutcome, Record, SamplingParams, ToolSchema, Value};
use hugr_host::{Capability, ChunkSink, estimate_text_tokens};
use hugr_toolkit::manifest::AgentDefinition;
use hugr_toolkit::runtime::{build_agent, trace_store_for};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[cfg(feature = "python")]
mod python;

pub const DEFAULT_MODEL: &str = "google/gemma-4-31B-it:cerebras";
pub const DEFAULT_BASE_URL: &str = "https://router.huggingface.co/v1";
pub const DEFAULT_TRACE_DIR: &str = ".hugr-docs-traces";
pub const DEFAULT_INPUT_USD_PER_M_TOKENS: f64 = 1.0;
pub const DEFAULT_OUTPUT_USD_PER_M_TOKENS: f64 = 1.5;
const DEFINITION_MANIFEST: &str = include_str!("../definition/hugr.toml");
const DEFINITION_SYSTEM: &str = include_str!("../definition/SYSTEM.md");

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

/// Exact phrasing the system prompt tells the model to emit when the docs do not
/// contain enough evidence. `build_answer` matches this (case-insensitively, after
/// trim) to mark a run [`DocsStatus::OffTopic`] while still surfacing the phrase
/// as `message`.
pub const NOT_FOUND_MESSAGE: &str = "It is not possible to find an answer in the docs.";

pub const SYSTEM_PROMPT: &str = "\
You are a documentation retrieval agent. Answer the user's question using only the documentation available through the provided read-only tools. Start by using docs_search, docs_list, or docs_outline to plan retrieval, then read every source document needed to support the answer. Decompose compound questions into facets and gather evidence for every facet before answering; if a question asks about multiple concepts, comparisons, constraints, or how mechanisms differ, do not stop after the first relevant document. Prefer docs_read_many or docs_read_range_many when several sources look relevant. AI_INDEX.md files are navigation aids only: use them to decide what to read, but never cite them as related documents. If the docs do not contain enough evidence for any facet, say what cannot be found in the docs instead of filling gaps from prior knowledge. Do not use prior knowledge. Your final response must be a single JSON object with exactly these fields: answer (string) and related_documents (array of document paths relative to the docs root, excluding AI_INDEX.md).";

#[derive(Clone, Debug)]
pub struct DocsConfig {
    pub root: PathBuf,
    pub trace_dir: PathBuf,
    pub trace_id: Option<String>,
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub input_usd_per_m_tokens: f64,
    pub output_usd_per_m_tokens: f64,
    pub sampling: SamplingParams,
}

#[non_exhaustive]
#[derive(Clone, Debug, Default)]
pub struct DocsConfigOptions {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub trace_id: Option<String>,
    pub trace_dir: Option<PathBuf>,
    pub input_usd_per_m_tokens: Option<f64>,
    pub output_usd_per_m_tokens: Option<f64>,
}

impl DocsConfigOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    pub fn with_trace_dir(mut self, trace_dir: impl Into<PathBuf>) -> Self {
        self.trace_dir = Some(trace_dir.into());
        self
    }

    pub fn with_input_usd_per_m_tokens(mut self, price: f64) -> Self {
        self.input_usd_per_m_tokens = Some(price);
        self
    }

    pub fn with_output_usd_per_m_tokens(mut self, price: f64) -> Self {
        self.output_usd_per_m_tokens = Some(price);
        self
    }
}

impl DocsConfig {
    pub fn from_env(root: PathBuf, model_override: Option<String>) -> Result<Self> {
        Self::from_options(
            root,
            DocsConfigOptions {
                model: model_override,
                ..DocsConfigOptions::default()
            },
        )
    }

    pub fn from_options(root: PathBuf, options: DocsConfigOptions) -> Result<Self> {
        let api_key = options
            .api_key
            .or_else(|| std::env::var("HUGR_DOCS_API_KEY").ok())
            .context("pass api_key or set HUGR_DOCS_API_KEY")?;
        let model = options
            .model
            .or_else(|| std::env::var("HUGR_DOCS_MODEL").ok())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let base_url = options
            .base_url
            .or_else(|| std::env::var("HUGR_DOCS_BASE_URL").ok())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let trace_dir = options
            .trace_dir
            .or_else(|| std::env::var_os("HUGR_DOCS_TRACE_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TRACE_DIR));
        let input_usd_per_m_tokens = options.input_usd_per_m_tokens.map_or_else(
            || {
                parse_env_f64(
                    "HUGR_DOCS_INPUT_USD_PER_M_TOKENS",
                    DEFAULT_INPUT_USD_PER_M_TOKENS,
                )
            },
            validate_price,
        )?;
        let output_usd_per_m_tokens = options.output_usd_per_m_tokens.map_or_else(
            || {
                parse_env_f64(
                    "HUGR_DOCS_OUTPUT_USD_PER_M_TOKENS",
                    DEFAULT_OUTPUT_USD_PER_M_TOKENS,
                )
            },
            validate_price,
        )?;
        let sampling = SamplingParams::new().with_temperature(0.0);
        Ok(Self {
            root,
            trace_dir,
            trace_id: options
                .trace_id
                .or_else(|| std::env::var("HUGR_DOCS_TRACE_ID").ok()),
            model,
            base_url,
            api_key,
            input_usd_per_m_tokens,
            output_usd_per_m_tokens,
            sampling,
        })
    }
}

fn validate_price(value: f64) -> Result<f64> {
    anyhow::ensure!(
        value.is_finite() && value >= 0.0,
        "token price must be a finite non-negative number"
    );
    Ok(value)
}

fn parse_env_f64(name: &str, default: f64) -> Result<f64> {
    match std::env::var(name) {
        Ok(value) => validate_price(
            value
                .parse::<f64>()
                .with_context(|| format!("parsing {name}={value:?}"))?,
        )
        .with_context(|| format!("parsing {name}={value:?}")),
        Err(_) => Ok(default),
    }
}

pub async fn answer_question(config: DocsConfig, question: &str) -> Result<DocsAnswer> {
    let started = Instant::now();
    match answer_question_inner(&config, question, started).await {
        Ok(answer) => Ok(answer),
        Err(error) => Ok(failure_answer(
            &config.model,
            &config.base_url,
            started.elapsed(),
            error.to_string(),
        )),
    }
}

async fn answer_question_inner(
    config: &DocsConfig,
    question: &str,
    started: Instant,
) -> Result<DocsAnswer> {
    anyhow::ensure!(!question.trim().is_empty(), "question cannot be empty");
    let docs = DocsRoot::new(&config.root)?;
    let definition = docs_definition(config, docs.root())?;
    let store = trace_store_for(&definition);
    let (agent, _warnings) = build_agent(&definition)
        .await
        .context("building docs agent from definition")?;

    let ask = Ask {
        question: user_prompt(question),
        trace_id: config.trace_id.clone().map(TraceId::new),
        ..Ask::default()
    };
    let agent_answer = agent.ask(ask).await?;
    let trace = store.get(&agent_answer.trace_id)?;

    let mut docs_answer = build_answer(trace.log.as_slice(), config, started.elapsed())
        .context("building JSON answer from Hugr session")
        .map(|mut answer| {
            answer.trace_id = Some(agent_answer.trace_id.to_string());
            answer.metadata.tokens_in = agent_answer.metadata.tokens_in;
            answer.metadata.tokens_out = agent_answer.metadata.tokens_out;
            answer.metadata.estimated_cost_micro_usd = agent_answer.metadata.cost_micro_usd;
            answer.metadata.model_calls = agent_answer.metadata.model_calls as usize;
            answer.metadata.tool_calls = agent_answer.metadata.tool_calls as usize;
            answer
        })?;
    if agent_answer.status == hugr_agent::STATUS_ERROR {
        docs_answer.status = DocsStatus::Error;
        docs_answer.message = agent_answer.message;
    }
    Ok(docs_answer)
}

fn docs_definition(config: &DocsConfig, docs_root: &Path) -> Result<AgentDefinition> {
    let mut definition =
        AgentDefinition::parse(DEFINITION_MANIFEST, "hugr-docs/definition/hugr.toml")
            .context("parsing embedded docs definition")?;
    definition.source_dir = Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("definition"));
    definition.system_prompt = Some(DEFINITION_SYSTEM.to_string());
    definition.agent.version = env!("CARGO_PKG_VERSION").to_string();
    definition.models.base_url = Some(config.base_url.clone());
    definition.models.api_key_env = Some("HUGR_DOCS_API_KEY".to_string());
    definition.provider_api_key = Some(config.api_key.clone());
    if let Some(tier) = definition.models.tiers.get_mut("docs") {
        tier.model = config.model.clone();
        tier.input_usd_per_m_tokens = Some(config.input_usd_per_m_tokens);
        tier.output_usd_per_m_tokens = Some(config.output_usd_per_m_tokens);
        tier.temperature = config.sampling.temperature.map(f64::from);
        tier.max_tokens = config.sampling.max_tokens;
    }
    let root = docs_root.display().to_string();
    for grant in &mut definition.tools {
        if grant.kind == hugr_toolkit::ToolKind::Library && grant.name == "fs_read" {
            grant.config = json!({ "root": root });
        }
    }
    definition.traces.store = Some(config.trace_dir.display().to_string());
    Ok(definition)
}

/// Build a docs answer from raw inputs, swallowing every error into a
/// [`DocsStatus::Error`] [`DocsAnswer`] so callers (notably the Python binding)
/// always receive a uniform result dict and never have to catch process failures.
///
/// Config-build failures (missing API key, bad price, …) are reported with the
/// default model/endpoint in `metadata`; everything else is reported with the
/// resolved config. This is the entry point the CLI and Python binding use.
pub async fn answer_with_options(
    root: PathBuf,
    options: DocsConfigOptions,
    question: &str,
) -> Result<DocsAnswer> {
    let started = Instant::now();
    let config = match DocsConfig::from_options(root, options) {
        Ok(config) => config,
        Err(error) => {
            return Ok(failure_answer(
                DEFAULT_MODEL,
                DEFAULT_BASE_URL,
                started.elapsed(),
                error.to_string(),
            ));
        }
    };
    answer_question(config, question).await
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

/// The outcome of a docs-retrieval run, serialized as a lowercase string so the
/// Python side can branch on `result["status"]` instead of guessing from `bool`.
///
/// - [`DocsStatus::Success`] — `success`: the model produced a real answer.
/// - [`DocsStatus::OffTopic`] — `off_topic`: the docs lacked evidence; the model
///   emitted [`NOT_FOUND_MESSAGE`] and `message` carries that phrase.
/// - [`DocsStatus::Error`] — `error`: an error stopped the run before a final
///   answer; `message` carries the error text.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DocsStatus {
    Success,
    OffTopic,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct DocsAnswer {
    /// See [`DocsStatus`].
    pub status: DocsStatus,
    /// The answer on success; the [`NOT_FOUND_MESSAGE`] phrasing when the docs
    /// lacked evidence; the error message/stacktrace when an error stopped the run.
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
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
    build_answer_with_reads(log, config, elapsed, ReadSets::default())
}

/// Build a `DocsAnswer` from a finished Hugr session.
///
/// Three outcomes, all returned as `Ok(DocsAnswer)` so the Python binding can
/// surface every case through `status`/`message` instead of raising:
///
/// - real answer → [`DocsStatus::Success`], `message` = the answer.
/// - model emitted [`NOT_FOUND_MESSAGE`] → [`DocsStatus::OffTopic`], `message`
///   = that phrase.
/// - no final model text (error stopped the run) → [`DocsStatus::Error`],
///   `message` = the recorded terminal error (or a fallback summary),
///   `related_documents` and token/usage metadata are still populated from
///   whatever the log holds.
///
/// `read_override` lets callers inject the read-document set (used when a
/// pre-amble error happens before the run produces any reads); pass
/// [`ReadSets::default()`] for the normal path.
fn build_answer_with_reads(
    log: &[hugr_core::LogEntry],
    config: &DocsConfig,
    elapsed: Duration,
    read_override: ReadSets,
) -> Result<DocsAnswer> {
    let read = if read_override.documents.is_empty() && read_override.indexes.is_empty() {
        read_document_sets(log)
    } else {
        read_override
    };
    let (tokens_in, tokens_out, model_calls, tool_calls) = usage_totals(log);
    let estimated_cost_micro_usd = estimate_cost_micro_usd(
        tokens_in,
        tokens_out,
        config.input_usd_per_m_tokens,
        config.output_usd_per_m_tokens,
    );
    let metadata = RunMetadata {
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
    };

    let final_text = log.iter().rev().find_map(|entry| match &entry.record {
        Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
            Some(output.text.as_str())
        }
        _ => None,
    });

    let Some(final_text) = final_text else {
        return Ok(DocsAnswer {
            status: DocsStatus::Error,
            message: missing_final_answer_message(log),
            trace_id: None,
            related_documents: sanitize_related_documents(Vec::new(), &read.documents),
            metadata,
        });
    };

    let payload = AnswerPayload::from_model_text(final_text);
    let related_documents = sanitize_related_documents(payload.related_documents, &read.documents);
    let status = if is_not_found_message(&payload.answer) {
        DocsStatus::OffTopic
    } else {
        DocsStatus::Success
    };
    Ok(DocsAnswer {
        status,
        message: payload.answer,
        trace_id: None,
        related_documents,
        metadata,
    })
}

fn is_not_found_message(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(NOT_FOUND_MESSAGE)
}

/// Construct a [`DocsStatus::Error`] answer for a process error that stopped the
/// run before a final model answer (config build failure, docs root missing,
/// engine panic, …). The error text lands in `message`; metadata is populated
/// with the resolved model/endpoint and a zeroed spend because no tokens were
/// consumed.
fn failure_answer(model: &str, endpoint: &str, elapsed: Duration, message: String) -> DocsAnswer {
    DocsAnswer {
        status: DocsStatus::Error,
        message,
        trace_id: None,
        related_documents: Vec::new(),
        metadata: RunMetadata {
            model: model.to_string(),
            endpoint: endpoint.to_string(),
            elapsed_ms: elapsed.as_millis(),
            tokens_in: 0,
            tokens_out: 0,
            estimated_cost_micro_usd: 0,
            input_usd_per_m_tokens: 0.0,
            output_usd_per_m_tokens: 0.0,
            model_calls: 0,
            tool_calls: 0,
            read_documents: 0,
            read_indexes: 0,
        },
    }
}

fn missing_final_answer_message(log: &[hugr_core::LogEntry]) -> String {
    if let Some(message) = last_terminal_error(log) {
        return format!("model did not produce a final answer; {message}");
    }
    let model_outputs = log
        .iter()
        .filter(|entry| matches!(entry.record, Record::ModelOutput { .. }))
        .count();
    let tool_results = log
        .iter()
        .filter(|entry| matches!(entry.record, Record::ToolResult { .. }))
        .count();
    format!(
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
            "docs_read" | "docs_read_range" | "fs_read" | "fs_read_range" => {
                collect_read_path(&mut read, result);
            }
            "docs_read_many"
            | "docs_read_range_many"
            | "docs_outline"
            | "fs_read_many"
            | "fs_outline" => {
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
        "Question: {question}\n\nBefore answering, make sure each distinct part of the question is backed by at least one read non-index document. Return only the final JSON object after using the docs tools. If the answer is absent from the docs, use this answer exactly: \"{NOT_FOUND_MESSAGE}\""
    )
}

pub fn prompt_est_tokens(question: &str) -> u32 {
    estimate_text_tokens(&user_prompt(question))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_core::{LogEntry, ModelOutput, OpId, Seq, Timestamp};

    #[test]
    fn parses_json_from_fenced_model_text() {
        let payload = AnswerPayload::from_model_text(
            "```json\n{\"answer\":\"A\",\"related_documents\":[\"hub/notifications.md\"]}\n```",
        );
        assert_eq!(payload.answer, "A");
        assert_eq!(payload.related_documents, vec!["hub/notifications.md"]);
    }

    #[test]
    fn config_options_use_explicit_values_without_env() {
        let config = DocsConfig::from_options(
            PathBuf::from("/docs"),
            DocsConfigOptions::new()
                .with_api_key("key")
                .with_base_url("https://example.test/v1")
                .with_model("provider/model")
                .with_input_usd_per_m_tokens(2.5)
                .with_output_usd_per_m_tokens(7.0),
        )
        .unwrap();

        assert_eq!(config.root, PathBuf::from("/docs"));
        assert_eq!(config.api_key, "key");
        assert_eq!(config.base_url, "https://example.test/v1");
        assert_eq!(config.model, "provider/model");
        assert_eq!(config.input_usd_per_m_tokens, 2.5);
        assert_eq!(config.output_usd_per_m_tokens, 7.0);

        let err = DocsConfig::from_options(
            PathBuf::from("/docs"),
            DocsConfigOptions::new()
                .with_api_key("key")
                .with_input_usd_per_m_tokens(f64::NAN),
        )
        .unwrap_err();
        assert!(err.to_string().contains("token price"));
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
        let log = vec![LogEntry::new(
            Seq(0),
            Timestamp(0),
            serde_json::from_value(json!({
                "OpEnded": {
                    "op": 7,
                    "outcome": { "Error": { "message": "provider rejected tools" } },
                    "meta": {
                        "started_at": 0,
                        "ended_at": 1,
                        "model": "docs",
                        "usage": null,
                        "extra": null
                    }
                }
            }))
            .unwrap(),
        )];
        let message = missing_final_answer_message(&log);
        assert!(message.contains("model operation 7 failed: provider rejected tools"));
    }

    fn test_config() -> DocsConfig {
        DocsConfig {
            root: PathBuf::from("/docs"),
            trace_dir: PathBuf::from(".hugr-docs-traces"),
            trace_id: None,
            model: "provider/model".to_string(),
            base_url: "https://example.test/v1".to_string(),
            api_key: "key".to_string(),
            input_usd_per_m_tokens: 1.0,
            output_usd_per_m_tokens: 1.5,
            sampling: SamplingParams::new().with_temperature(0.0),
        }
    }

    fn model_output_entry(seq: u64, text: &str) -> LogEntry {
        LogEntry::new(
            Seq(seq),
            Timestamp(seq),
            Record::ModelOutput {
                op: OpId(seq),
                output: ModelOutput::text(text.to_string()),
                est_tokens: 0,
            },
        )
    }

    #[test]
    fn build_answer_success_for_real_answer() {
        let log = vec![model_output_entry(
            0,
            "```json\n{\"answer\":\"A\",\"related_documents\":[]}\n```",
        )];
        let config = test_config();
        let answer = build_answer(&log, &config, Duration::from_millis(10)).unwrap();
        assert_eq!(answer.status, DocsStatus::Success);
        assert_eq!(answer.message, "A");
        assert!(answer.related_documents.is_empty());
        assert_eq!(answer.metadata.model, "provider/model");
        assert_eq!(answer.metadata.model_calls, 0);
    }

    #[test]
    fn build_answer_off_topic_for_not_found_phrase() {
        let log = vec![model_output_entry(
            0,
            "```json\n{\"answer\":\"It is not possible to find an answer in the docs.\",\"related_documents\":[]}\n```",
        )];
        let config = test_config();
        let answer = build_answer(&log, &config, Duration::from_millis(10)).unwrap();
        assert_eq!(answer.status, DocsStatus::OffTopic);
        assert_eq!(answer.message, NOT_FOUND_MESSAGE);
    }

    #[test]
    fn build_answer_error_for_missing_final_answer() {
        let log = vec![LogEntry::new(
            Seq(0),
            Timestamp(0),
            serde_json::from_value(json!({
                "OpEnded": {
                    "op": 7,
                    "outcome": { "Error": { "message": "provider rejected tools" } },
                    "meta": {
                        "started_at": 0,
                        "ended_at": 1,
                        "model": "docs",
                        "usage": null,
                        "extra": null
                    }
                }
            }))
            .unwrap(),
        )];
        let config = test_config();
        let answer = build_answer(&log, &config, Duration::from_millis(10)).unwrap();
        assert_eq!(answer.status, DocsStatus::Error);
        assert!(
            answer
                .message
                .contains("model operation 7 failed: provider rejected tools"),
            "message was: {}",
            answer.message
        );
        assert!(answer.related_documents.is_empty());
    }

    #[tokio::test]
    async fn answer_with_options_surfaces_config_error_as_failure() {
        // A NaN token price is an unambiguous config-validation failure
        // (independent of any HUGR_DOCS_* env the test process may inherit).
        // The call still returns Ok with status=error and the error in `message`.
        let answer = answer_with_options(
            PathBuf::from("/does/not/exist"),
            DocsConfigOptions::new()
                .with_api_key("key")
                .with_input_usd_per_m_tokens(f64::NAN),
            "any question",
        )
        .await
        .unwrap();
        assert_eq!(answer.status, DocsStatus::Error);
        assert!(answer.message.contains("token price"));
        assert_eq!(answer.metadata.model, DEFAULT_MODEL);
        assert!(answer.related_documents.is_empty());
    }

    #[test]
    fn embedded_definition_is_toolkit_parseable_and_overridable() {
        let dir = test_docs_dir();
        let config = DocsConfig {
            root: dir.join("docs"),
            trace_dir: dir.join("traces"),
            trace_id: None,
            model: "provider/override".to_string(),
            base_url: "https://override.test/v1".to_string(),
            api_key: "key".to_string(),
            input_usd_per_m_tokens: 2.0,
            output_usd_per_m_tokens: 3.0,
            sampling: SamplingParams::new().with_temperature(0.0),
        };
        let root = DocsRoot::new(&config.root).unwrap();
        let def = docs_definition(&config, root.root()).unwrap();
        assert_eq!(def.agent.name, "hugr-docs");
        assert_eq!(
            def.models.base_url.as_deref(),
            Some("https://override.test/v1")
        );
        assert_eq!(def.models.tiers["docs"].model, "provider/override");
        assert_eq!(
            def.traces.store.as_deref(),
            Some(dir.join("traces").to_str().unwrap())
        );
        assert_eq!(def.tools[0].name, "fs_read");
        assert_eq!(
            def.tools[0].config["root"],
            root.root().display().to_string()
        );
        assert!(def.system_prompt.unwrap().contains("fs_search"));

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn docs_status_serializes_as_snake_case_strings() {
        // The Python side branches on `result["status"]`; pin the exact strings
        // so a serde-attribute change can't silently break the contract.
        assert_eq!(
            serde_json::to_string(&DocsStatus::Success).unwrap(),
            "\"success\""
        );
        assert_eq!(
            serde_json::to_string(&DocsStatus::OffTopic).unwrap(),
            "\"off_topic\""
        );
        assert_eq!(
            serde_json::to_string(&DocsStatus::Error).unwrap(),
            "\"error\""
        );
        // And round-trips back.
        assert_eq!(
            serde_json::from_str::<DocsStatus>("\"off_topic\"").unwrap(),
            DocsStatus::OffTopic
        );
    }

    #[test]
    fn docs_answer_serializes_trace_id_when_present() {
        let answer = DocsAnswer {
            status: DocsStatus::Success,
            message: "A".to_string(),
            trace_id: Some("trace-1".to_string()),
            related_documents: Vec::new(),
            metadata: RunMetadata {
                model: "provider/model".to_string(),
                endpoint: "https://example.test/v1".to_string(),
                elapsed_ms: 1,
                tokens_in: 2,
                tokens_out: 3,
                estimated_cost_micro_usd: 4,
                input_usd_per_m_tokens: 1.0,
                output_usd_per_m_tokens: 1.5,
                model_calls: 1,
                tool_calls: 0,
                read_documents: 0,
                read_indexes: 0,
            },
        };
        let value = serde_json::to_value(answer).unwrap();
        assert_eq!(value["trace_id"], "trace-1");
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

    #[test]
    fn read_document_sets_counts_definition_fs_tools() {
        let log = vec![
            tool_result(0, "fs_read_range", json!({ "path": "guide/a.md" })),
            tool_result(
                1,
                "fs_read_many",
                json!({
                    "documents": [
                        { "path": "guide/b.md" },
                        { "path": "AI_INDEX.md" }
                    ]
                }),
            ),
            tool_result(
                2,
                "fs_outline",
                json!({
                    "documents": [
                        { "path": "guide/c.md", "headings": [] }
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
        LogEntry::new(
            Seq(seq),
            Timestamp(seq),
            Record::ToolResult {
                op: OpId(seq),
                name: name.to_string(),
                call_id: format!("call-{seq}"),
                result,
                est_tokens: 0,
            },
        )
    }
}
