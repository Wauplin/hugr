use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use hugr_agent::{Ask, TraceId};
use hugr_core::{OpMeta, OpOutcome, Record, SamplingParams, Value};
use hugr_host::estimate_text_tokens;
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

    fn root(&self) -> &Path {
        self.root.as_path()
    }
}

fn is_ai_index(path: &str) -> bool {
    Path::new(path)
        .file_name()
        .is_some_and(|name| name == "AI_INDEX.md")
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
    use std::fs;

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
