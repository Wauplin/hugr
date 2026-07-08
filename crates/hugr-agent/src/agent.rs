//! The first real `Agent::ask` path — resume & fork semantics
//! (ARCHITECTURE §19.2, ROADMAP T0.3).
//!
//! An [`Agent`] is a reusable configuration (system prompt, model adapters,
//! capabilities, permission policy) plus a [`TraceStore`]. Each
//! [`ask`](Agent::ask) builds a **fresh** engine:
//!
//! - `trace_id: None` → a fresh brain runs the turn and the session persists
//!   as a new **root** trace.
//! - `trace_id: Some(parent)` → the parent trace is loaded from the store and
//!   **re-folded** into the fresh brain via [`EngineBuilder::resume`] — zero IO
//!   beyond the one file read, no model/tool re-calls (ARCHITECTURE §15.1) —
//!   then the new question runs as a live turn and the whole session (old +
//!   new events) persists as a **new** trace with `depends_on = parent`.
//!
//! The parent file is never touched, so forking is just asking the same
//! parent twice: the two children are sibling traces in the store's DAG.
//!
//! Error discipline (§18.1): *run* failures — the model erroring, no final
//! answer — are **answers** (`status: Error`) with a persisted trace, so the
//! caller still gets a `trace_id` to inspect. Only *infrastructure* failures
//! (an unknown parent id, a store write error) return [`AskError`]; surfaces
//! convert those to error answers at their own boundary (T0.8).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use hugr_core::{LogEntry, ModelSelector, Record, SamplingParams, ToolSchema};
use hugr_host::{Capability, Clock, Engine, Frontend, ModelAdapter};
use hugr_replay::{BlobStore, Trace};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_tool::{AgentTool, AgentToolSpec};
use crate::blobs::{self, BlobError};
use crate::contract::{Answer, AnswerMeta, Ask, STATUS_ERROR, STATUS_SUCCESS, TraceId};
use crate::limits::{LimitState, LimitedAdapter};
use crate::scratch::{ScratchDir, copy_tree, scratch_tool_schemas};
use crate::store::{StoreError, TraceHead, TraceHeader, TraceStore};

/// Default name of the scratch subtree directory, placed next to the trace
/// files inside the store root. Hidden and non-`.json`, so `TraceStore::list`
/// skips it.
const DEFAULT_SCRATCH_DIRNAME: &str = ".scratch";

/// Working subtrees (one per in-flight ask) live under this child of the
/// scratch root until the ask's trace is persisted and the copy is finalized to
/// its own `<trace_id>` subtree.
const PENDING_DIRNAME: &str = ".pending";

/// Default name of the content-addressed blob store directory, placed next to
/// the trace files inside the store root (ROADMAP T0.5). Hidden and
/// non-`.json`, so `TraceStore::list` skips it — a store dir carries its
/// agents' outbound blobs alongside their traces.
const DEFAULT_BLOBS_DIRNAME: &str = ".blobs";

/// A configured subagent: ask it questions, get [`Answer`]s, resume or fork
/// any stored trace. Construct with [`Agent::new`], then set the public fields
/// (the toolkit's `build_agent` is the one assembly path).
///
/// Cheap to share pieces: adapters, capabilities, and the policy are `Arc`s,
/// so each ask assembles a fresh engine without re-constructing them.
pub struct Agent {
    pub name: String,
    pub version: String,
    pub description: String,
    pub store: TraceStore,
    pub system_prompt: Option<String>,
    pub models: Vec<(ModelSelector, Arc<dyn ModelAdapter>)>,
    pub default_model: Option<ModelSelector>,
    pub capabilities: Vec<Arc<dyn Capability>>,
    pub sampling: Option<SamplingParams>,
    pub clock: Option<Clock>,
    /// Root of the per-lineage scratchpad subtree (ARCHITECTURE §19.3).
    pub scratch_root: PathBuf,
    /// Content-addressed store outbound blobs land in (ARCHITECTURE §18.3).
    pub blob_store: BlobStore,
    /// Per-tier pricing used to derive `AnswerMeta.cost_micro_usd` from
    /// trace-recorded usage (ARCHITECTURE §18.4). Missing tiers price at zero.
    pub pricing: Pricing,
    pub limits: AgentLimits,
    /// Optional response JSON Schema. When set, the final model text must parse
    /// as a JSON object and validate before it becomes `Answer.response`.
    pub response_schema: Option<Value>,
    /// Granted child agents exposed as ordinary `agent_<name>` capabilities
    /// (ARCHITECTURE §20.5, ROADMAP T3.8). Registered fresh per ask so each
    /// invocation's child cost folds into this ask's `AnswerMeta`.
    pub agent_tools: Vec<AgentToolSpec>,
    /// Monotonic counter naming each ask's pending working directory — the one
    /// piece of host-side nondeterminism, kept off the trace (scratch content
    /// never enters the log; results carry only relative paths).
    next_scratch: Arc<AtomicU64>,
}

impl Agent {
    /// A fresh agent with defaults. `name`/`version` are stamped into every
    /// trace header; `store` is where the immutable traces live. The scratch
    /// and blob roots default to hidden subtrees inside the store root; set
    /// the public fields to override anything.
    pub fn new(name: impl Into<String>, version: impl Into<String>, store: TraceStore) -> Agent {
        let scratch_root = store.root().join(DEFAULT_SCRATCH_DIRNAME);
        let blob_store = BlobStore::new(store.root().join(DEFAULT_BLOBS_DIRNAME));
        Agent {
            name: name.into(),
            version: version.into(),
            description: String::new(),
            store,
            system_prompt: None,
            models: Vec::new(),
            default_model: None,
            capabilities: Vec::new(),
            sampling: None,
            clock: None,
            scratch_root,
            blob_store,
            pricing: Pricing::default(),
            limits: AgentLimits::default(),
            response_schema: None,
            agent_tools: Vec::new(),
            next_scratch: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The trace store this agent persists into.
    pub fn store(&self) -> &TraceStore {
        &self.store
    }

    /// The content-addressed blob store this agent's outbound blobs land in
    /// (ARCHITECTURE §18.3). An orchestrator resolves an [`Answer`] blob's
    /// `sha256` ref through here.
    pub fn blob_store(&self) -> &BlobStore {
        &self.blob_store
    }

    /// Describe this agent's public card: identity, tools + privileges, model
    /// tiers, pricing, and declared limits (ARCHITECTURE §18.2).
    pub fn describe(&self) -> AgentCard {
        let mut tools: Vec<ToolCard> = scratch_tool_schemas()
            .into_iter()
            .map(|schema| ToolCard {
                name: schema.name.clone(),
                description: schema.description.clone(),
                privilege: "scratchpad".to_string(),
                runs_in_background: false,
                schema,
                scope: json!({ "root": self.scratch_root.display().to_string() }),
            })
            .collect();
        // Granted child agents (§20.5) show as `agent_<name>` tools; they are
        // registered per-ask but are part of the agent's advertised surface.
        tools.extend(self.agent_tools.iter().map(|spec| {
            let schema = spec.schema();
            ToolCard {
                name: schema.name.clone(),
                description: schema.description.clone(),
                privilege: "agent".to_string(),
                runs_in_background: false,
                schema,
                scope: Value::Null,
            }
        }));
        tools.extend(self.capabilities.iter().map(|capability| {
            let schema = capability.schema();
            ToolCard {
                name: schema.name.clone(),
                description: schema.description.clone(),
                privilege: if capability.requires_permission() {
                    "gated".to_string()
                } else {
                    "read_only".to_string()
                },
                runs_in_background: capability.runs_in_background(),
                schema,
                scope: Value::Null,
            }
        }));
        tools.sort_by(|a, b| a.name.cmp(&b.name));

        AgentCard {
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            tools,
            model_tiers: self.model_tiers(),
            limits: self.limits.clone(),
        }
    }

    /// List stored trace headers for this agent. This is the same cheap
    /// header-only read as [`TraceStore::list`].
    pub fn traces(&self) -> Result<Vec<TraceHead>, StoreError> {
        self.store.list()
    }

    /// Run one ask to completion (ARCHITECTURE §18.1/§19.2). See the module
    /// docs for the fresh-vs-resume split and the error discipline.
    pub async fn ask(&self, ask: Ask) -> Result<Answer, AskError> {
        let started = Instant::now();
        let parent = ask.trace_id.clone();

        // Assemble a fresh engine per ask. Recording is always on: the trace
        // *is* the product here.
        let mut builder = Engine::builder()
            .record(true)
            .frontend(Box::new(SilentFrontend));
        // Limits enforcement (§18/§20.1, ROADMAP T3.1): the counting/cost limits
        // wrap each model adapter so a call over budget is refused (and folded
        // as an ordinary `ModelError`); the wall-clock timeout wraps the turn
        // below. Both surface as an error *answer* with a persisted trace.
        let limit_state = LimitState::new(self.limits.clone(), self.pricing.clone());
        let wrap = limit_state.needs_adapter_wrap();
        for (selector, adapter) in &self.models {
            let adapter: Arc<dyn ModelAdapter> = if wrap {
                LimitedAdapter::new(
                    selector_name(selector),
                    adapter.clone(),
                    limit_state.clone(),
                )
            } else {
                adapter.clone()
            };
            builder = builder.model(selector.clone(), adapter);
        }
        if let Some(selector) = &self.default_model {
            builder = builder.default_model(selector.clone());
        }
        for capability in &self.capabilities {
            builder = builder.capability(capability.clone());
        }
        if let Some(system) = &self.system_prompt {
            builder = builder.system_prompt(system.clone());
        }
        if let Some(sampling) = &self.sampling {
            builder = builder.sampling(sampling.clone());
        }
        if let Some(clock) = &self.clock {
            builder = builder.clock(clock.clone());
        }
        let parent_trace = match &parent {
            Some(parent_id) => Some(self.store.get(parent_id)?),
            None => None,
        };

        // Agent-as-tool grants (§20.5, T3.8): register each granted child agent
        // as an `agent_<name>` capability with a per-ask spend sink, so its cost
        // folds into *this* ask's meta after the turn.
        let child_spend: Arc<Mutex<Vec<AnswerMeta>>> = Arc::new(Mutex::new(Vec::new()));
        for spec in &self.agent_tools {
            builder = builder.capability(Arc::new(AgentTool::new(spec, child_spend.clone())));
        }

        if let Some(trace) = parent_trace {
            // Re-fold the parent's recorded events into the fresh brain — no
            // model or tool is ever re-run for work that already happened
            // (§15.1); `resume` only rebuilds state.
            builder = builder.resume(trace);
        }

        // Per-lineage scratchpad (§19.3): a fresh working subtree, seeded by
        // copying the parent's finalized subtree on resume/fork — so this ask
        // sees the ancestor's notes but never a sibling's writes.
        let (scratch, working_dir) = self.prepare_scratch(parent.as_ref())?;
        for capability in scratch.capabilities() {
            builder = builder.capability(capability);
        }

        // Materialize inbound blobs into the working scratch dir *before* the
        // turn, with declared perms, so tools see plain files in the jail
        // (§18.3). Malformed hand-ins are infra errors (AskError), not answers.
        blobs::materialize_inbound(&working_dir, &ask.blobs, &self.blob_store)?;

        let mut engine = builder.build();

        // Accounting baseline: on resume the brain's log already holds the
        // parent's entries; this ask's meta must cover only the new turn.
        let baseline = engine.brain().state().log().len();

        // Drive the turn, bounded by the wall-clock timeout when one is set. On
        // elapse the turn future is dropped mid-flight; the recorded event
        // prefix is self-consistent, so the persisted (partial) trace still
        // replays. `session_end` then flushes the final checkpoint/render.
        match self.limits.timeout_ms {
            Some(ms) if ms > 0 => {
                if tokio::time::timeout(
                    std::time::Duration::from_millis(ms),
                    engine.user_turn(ask.question.clone()),
                )
                .await
                .is_err()
                {
                    limit_state.record_timeout(ms);
                }
            }
            _ => engine.user_turn(ask.question.clone()).await,
        }
        engine.session_end();

        let log = engine.brain().state().log();
        // A limit trip supersedes the log-derived answer: it is an error answer
        // with a typed reason on `extra` (ROADMAP T3.1). Otherwise the final
        // model output is the answer (§18.1).
        let trip = limit_state.trip();
        let (status, response, extra) = match &trip {
            Some(trip) => (
                STATUS_ERROR.to_string(),
                error_response(trip.message(), Value::Null),
                trip.extra(),
            ),
            None => {
                let (status, response) = final_response(log, self.response_schema.as_ref());
                (status, response, Value::Null)
            }
        };

        // Persist old + new as one NEW immutable trace; the parent file is
        // never mutated — lineage lives in `depends_on` (§19.2).
        let trace = engine
            .trace()
            .expect("recording is always enabled on an agent engine");
        let mut metadata = meta_from_trace(
            &trace,
            baseline,
            started.elapsed().as_millis() as u64,
            &self.pricing,
        );
        // Fold each delegated child agent's spend into this ask's meta (§20.5).
        for child_meta in child_spend.lock().unwrap().iter() {
            metadata.merge_child(child_meta);
        }
        let mut header = TraceHeader::new(&self.name, &self.version, &ask.question, &status);
        if let Some(parent_id) = parent {
            header = header.with_depends_on(parent_id);
        }
        let trace_id = self.store.put(trace, header)?;

        // Sweep produced files (the `out/` scratch subtree) into the
        // content-addressed store and return them as outbound blobs (§18.3).
        // Done before finalize while the working subtree is still in place;
        // dedup by hash lives in the store.
        let out_blobs = blobs::sweep_outbound(&working_dir, &self.blob_store)?;

        // Finalize the working subtree under the new trace's id so a later
        // resume/fork of *this* trace can seed from it (§19.3). Scratch is
        // never recorded, so this move happens after the trace is persisted.
        self.finalize_scratch(&working_dir, &trace_id)?;

        Ok(Answer {
            status,
            response,
            trace_id,
            blobs: out_blobs,
            metadata,
            extra,
        })
    }

    /// Create this ask's working scratch directory (a fresh `.pending/<n>`
    /// subtree) and, when resuming a parent, seed it with a copy of the
    /// parent's finalized subtree (copy-on-fork, §19.3). Returns the jailed
    /// [`ScratchDir`] for the tools plus the working path for finalization.
    fn prepare_scratch(&self, parent: Option<&TraceId>) -> Result<(ScratchDir, PathBuf), AskError> {
        let n = self.next_scratch.fetch_add(1, Ordering::SeqCst);
        let working = self
            .scratch_root
            .join(PENDING_DIRNAME)
            .join(format!("{}-{n}", std::process::id()));
        // A stale working dir from a crashed prior run must not leak in.
        if working.exists() {
            std::fs::remove_dir_all(&working)?;
        }
        if let Some(parent_id) = parent {
            let parent_scratch = self.scratch_root.join(parent_id.as_str());
            if parent_scratch.exists() {
                copy_tree(&parent_scratch, &working)?;
            } else {
                std::fs::create_dir_all(&working)?;
            }
        } else {
            std::fs::create_dir_all(&working)?;
        }
        let scratch = ScratchDir::new(&working)?;
        Ok((scratch, working))
    }

    /// Move this ask's working subtree to its final `<trace_id>` home so the
    /// lineage persists. A same-filesystem rename (both are under the scratch
    /// root); trace ids are unique, but any pre-existing target is cleared
    /// first so the move can't fail on a stray directory.
    fn finalize_scratch(&self, working: &PathBuf, trace_id: &TraceId) -> Result<(), AskError> {
        let final_dir = self.scratch_root.join(trace_id.as_str());
        if final_dir.exists() {
            std::fs::remove_dir_all(&final_dir)?;
        }
        if let Some(parent) = final_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(working, &final_dir)?;
        Ok(())
    }

    fn model_tiers(&self) -> Vec<ModelTierCard> {
        let default = self.default_model.as_ref().map(selector_name).or_else(|| {
            self.models
                .first()
                .map(|(selector, _)| selector_name(selector))
        });
        let mut tiers: Vec<_> = self
            .models
            .iter()
            .map(|(selector, _)| {
                let selector = selector_name(selector);
                ModelTierCard {
                    default: default.as_ref() == Some(&selector),
                    pricing: self.pricing.price_for(&selector),
                    selector,
                }
            })
            .collect();
        tiers.sort_by(|a, b| a.selector.cmp(&b.selector));
        tiers
    }
}

/// Public description returned by [`Agent::describe`] (ROADMAP T0.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub version: String,
    pub description: String,
    pub tools: Vec<ToolCard>,
    pub model_tiers: Vec<ModelTierCard>,
    pub limits: AgentLimits,
}

/// One advertised tool plus the privilege metadata surfaces need.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCard {
    pub name: String,
    pub description: String,
    /// Coarse privilege label (`read_only` / `scratchpad` / `gated` / `agent`)
    /// — an open string set nothing branches on (§14).
    pub privilege: String,
    pub runs_in_background: bool,
    pub schema: ToolSchema,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub scope: Value,
}

/// One logical model tier exposed in the agent card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelTierCard {
    pub selector: String,
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<TierPrice>,
}

/// Declared runtime limits, enforced host-side on every ask (ROADMAP T3.1) and
/// exposed on the T0.7 introspection card. Each `None` field is unbounded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl AgentLimits {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_max_model_calls(mut self, max_model_calls: u32) -> Self {
        self.max_model_calls = Some(max_model_calls);
        self
    }

    pub fn with_max_cost_micro_usd(mut self, max_cost_micro_usd: u64) -> Self {
        self.max_cost_micro_usd = Some(max_cost_micro_usd);
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }
}

/// Per-tier token prices used by [`Agent`] cost accounting (ROADMAP T0.6).
/// Values are USD per million tokens, matching provider price sheets. The
/// computed answer cost is rounded to the nearest micro-USD.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pricing {
    tiers: BTreeMap<String, TierPrice>,
}

impl Pricing {
    /// No configured prices. Tokens and calls are still reported; cost is zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or replace one selector's pricing.
    pub fn with_tier(
        mut self,
        selector: impl Into<String>,
        input_usd_per_m_tokens: f64,
        output_usd_per_m_tokens: f64,
    ) -> Self {
        self.tiers.insert(
            selector.into(),
            TierPrice::new(input_usd_per_m_tokens, output_usd_per_m_tokens),
        );
        self
    }

    pub(crate) fn cost_micro_usd(
        &self,
        selector: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> u64 {
        let Some(price) = self.tiers.get(selector) else {
            return 0;
        };
        let cost = (input_tokens as f64 * price.input_usd_per_m_tokens)
            + (output_tokens as f64 * price.output_usd_per_m_tokens);
        cost.round().max(0.0) as u64
    }

    fn price_for(&self, selector: &str) -> Option<TierPrice> {
        self.tiers.get(selector).copied()
    }
}

/// One tier's input/output prices in USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TierPrice {
    pub input_usd_per_m_tokens: f64,
    pub output_usd_per_m_tokens: f64,
}

impl TierPrice {
    /// Construct one tier price line.
    pub fn new(input_usd_per_m_tokens: f64, output_usd_per_m_tokens: f64) -> Self {
        Self {
            input_usd_per_m_tokens,
            output_usd_per_m_tokens,
        }
    }
}

/// Infrastructure failures of an ask — everything that prevents a trace from
/// being persisted at all. Run failures are *answers*, not errors (§18.1).
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    /// The parent trace could not be loaded, or the new trace could not be
    /// persisted.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The per-lineage scratchpad subtree (§19.3) could not be prepared,
    /// seeded, or finalized on disk.
    #[error("scratchpad IO error: {0}")]
    Scratch(#[from] std::io::Error),

    /// An inbound blob could not be materialized, or an outbound blob could not
    /// be swept into the content-addressed store (§18.3).
    #[error("blob exchange error: {0}")]
    Blob(#[from] BlobError),
}

/// The parent id an [`AskError::Store`] not-found refers to, if any — a small
/// convenience for surfaces mapping infra errors to error answers.
impl AskError {
    pub fn missing_trace(&self) -> Option<&TraceId> {
        match self {
            AskError::Store(StoreError::NotFound { id }) => Some(id),
            _ => None,
        }
    }
}

/// Extract the final response from the durable log: the last model output with
/// no tool calls is the turn's answer. No text means the run failed before
/// answering — an error *answer* (§18.1), with the terminal error surfaced.
fn final_response(log: &[LogEntry], schema: Option<&Value>) -> (String, Value) {
    let final_text = log.iter().rev().find_map(|entry| match &entry.record {
        Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
            Some(output.text.clone())
        }
        _ => None,
    });
    match final_text {
        Some(text) => match parse_model_response(&text, schema) {
            Ok(response) => (STATUS_SUCCESS.to_string(), response),
            Err(error) => (
                STATUS_ERROR.to_string(),
                error_response(
                    format!("model response did not match the response contract: {error}"),
                    json!({ "raw": text }),
                ),
            ),
        },
        None => (
            STATUS_ERROR.to_string(),
            error_response(missing_final_answer_message(log), Value::Null),
        ),
    }
}

fn parse_model_response(text: &str, schema: Option<&Value>) -> Result<Value, String> {
    let trimmed = strip_json_fence(text.trim());
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) if value.is_object() => {
            if let Some(schema) = schema {
                validate_response_schema(schema, &value)?;
            }
            Ok(value)
        }
        Ok(_) => Err("final response JSON must be an object".to_string()),
        Err(_) if schema.is_none() => Ok(json!({ "text": text.trim() })),
        Err(error) => Err(format!("final response is not valid JSON: {error}")),
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

fn error_response(message: impl Into<String>, extra: Value) -> Value {
    let message = message.into();
    if extra.is_null() {
        json!({ "error": message })
    } else {
        json!({ "error": message, "details": extra })
    }
}

fn validate_response_schema(schema: &Value, value: &Value) -> Result<(), String> {
    let validator =
        jsonschema::validator_for(schema).map_err(|err| format!("schema is invalid: {err}"))?;
    validator.validate(value).map_err(|err| {
        let path = err.instance_path().to_string();
        if path.is_empty() {
            err.to_string()
        } else {
            format!("{err} at {path}")
        }
    })?;
    Ok(())
}

fn missing_final_answer_message(log: &[LogEntry]) -> String {
    let terminal_error = log.iter().rev().find_map(|entry| match &entry.record {
        Record::OpEnded {
            outcome: hugr_core::OpOutcome::Error(error),
            ..
        } => Some(error.to_string()),
        _ => None,
    });
    match terminal_error {
        Some(error) => format!("model did not produce a final answer; last error: {error}"),
        None => "model did not produce a final answer".to_string(),
    }
}

/// Accounting for this ask, folded from the *new* slice of the trace log (a
/// resumed ask never re-bills its ancestry): totals only, priced per model
/// call from the per-tier price sheet.
fn meta_from_trace(
    trace: &Trace,
    baseline: usize,
    duration_ms: u64,
    pricing: &Pricing,
) -> AnswerMeta {
    let mut meta = AnswerMeta {
        duration_ms,
        ..AnswerMeta::default()
    };
    for entry in &trace.log[baseline..] {
        let Record::OpEnded { meta: op, .. } = &entry.record else {
            continue;
        };
        if let (Some(selector), Some(usage)) = (&op.model, &op.usage) {
            meta.model_calls += 1;
            meta.tokens_in += usage.input_tokens;
            meta.tokens_out += usage.output_tokens;
            meta.cost_micro_usd +=
                pricing.cost_micro_usd(&selector.0, usage.input_tokens, usage.output_tokens);
        } else if op.model.is_none() {
            meta.tool_calls += 1;
        }
    }
    meta
}

fn selector_name(selector: &ModelSelector) -> String {
    selector.0.clone()
}

/// A no-op front-end: a subagent's product is its `Answer` + trace, not a
/// terminal render. Surfaces that want live output can grow a builder knob
/// later without touching the contract.
struct SilentFrontend;

impl Frontend for SilentFrontend {}
