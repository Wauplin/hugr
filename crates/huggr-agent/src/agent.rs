//! `Agent::ask` — resume & fork semantics.
//!
//! An [`Agent`] is a reusable configuration (system prompt, model adapters, capabilities, permission policy) plus a [`TraceStore`]. Each [`ask`](Agent::ask) builds a **fresh** engine:
//!
//! - `trace_id: None` → a fresh brain runs the turn and the session persists as a new **root** trace.
//! - `trace_id: Some(parent)` → the parent trace is loaded from the store and **re-folded** into the fresh brain — no model/tool re-calls — then the new question runs as a live turn and the whole session persists as a **new** trace with `depends_on = parent`.
//!
//! The parent file is never touched, so forking is just asking the same parent twice: the two children are sibling traces in the store's DAG.
//!
//! Error discipline: *run* failures — the model erroring, no final answer — are **answers** (`status: Error`) with a persisted trace, so the caller still gets a `trace_id` to inspect. Only *infrastructure* failures (an unknown parent id, a store write error) return [`AskError`]; surfaces convert those to error answers at their own boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use huggr_core::{
    BudgetPolicy, DoneReason, LogEntry, ModelSelector, OpId, OutputEvent, Record, ToolSchema, Usage,
};
use huggr_host::{Capability, Clock, Engine, Frontend, ModelAdapter};
use huggr_replay::{BlobStore, Trace};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::agent_tool::{AgentTool, AgentToolSpec};
use crate::analytics::{
    AgentStats, AnalyticsError, StatsOptions, TraceListing, collect_stats,
    list_traces_with_feedback,
};
use crate::blobs::{self, BlobBackend, BlobError, FsBlobStore};
use crate::contract::{Answer, AnswerMeta, Ask, STATUS_ERROR, STATUS_SUCCESS, TraceId};
use crate::feedback::{
    Feedback, FeedbackBackend, FeedbackError, FsFeedbackStore, MemFeedbackStore,
};
use crate::limits::{LimitState, LimitedAdapter};
use crate::scratch::{FsScratch, ScratchBackend, ScratchSession, scratch_tool_schemas};
use crate::skills::{discover_skills, skills_prompt};
use crate::store::{StoreError, TraceBackend, TraceHead, TraceHeader, TraceStore};

/// Default blob store directory inside the store root. Hidden and non-`.json`, so `TraceStore::list` skips it.
const DEFAULT_BLOBS_DIRNAME: &str = ".blobs";

/// Default scratch subtree directory for direct `Agent::new` users.
const DEFAULT_SCRATCH_DIRNAME: &str = "scratch";
const DEFAULT_FEEDBACK_DIRNAME: &str = ".feedback";

#[derive(Clone)]
pub struct StorageOverrides {
    pub traces: Arc<dyn TraceBackend>,
    pub blobs: Arc<dyn BlobBackend>,
    pub feedback: Arc<dyn FeedbackBackend>,
    pub scratch: Arc<dyn ScratchBackend>,
    pub scratch_scope: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AskStarted {
        trace_parent: Option<TraceId>,
    },
    ModelStarted {
        op: OpId,
        tier: String,
    },
    TextDelta {
        op: OpId,
        text: String,
    },
    ModelEnded {
        op: OpId,
        usage: Usage,
    },
    ToolStarted {
        op: OpId,
        name: String,
        args: Value,
    },
    ToolEnded {
        op: OpId,
        name: String,
        is_error: bool,
        result: Value,
    },
    Notice {
        message: String,
    },
    Done {
        reason: DoneReason,
    },
    AnswerReady {
        answer: Box<Answer>,
    },
}

impl std::fmt::Debug for StorageOverrides {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageOverrides")
            .field("scratch_scope", &self.scratch_scope)
            .finish_non_exhaustive()
    }
}

impl StorageOverrides {
    pub fn new(
        traces: Arc<dyn TraceBackend>,
        blobs: Arc<dyn BlobBackend>,
        scratch: Arc<dyn ScratchBackend>,
    ) -> Self {
        Self {
            traces,
            blobs,
            feedback: Arc::new(MemFeedbackStore::new()),
            scratch,
            scratch_scope: Value::Null,
        }
    }

    pub fn with_feedback(mut self, feedback: Arc<dyn FeedbackBackend>) -> Self {
        self.feedback = feedback;
        self
    }

    pub fn with_scratch_scope(mut self, scope: Value) -> Self {
        self.scratch_scope = scope;
        self
    }
}

/// A configured huglet: ask it questions, get [`Answer`]s, resume or fork
/// any stored trace. Construct with [`Agent::new`], then set the public fields
/// (the toolkit's `build_agent` is the one assembly path).
///
/// Cheap to share pieces: adapters, capabilities, and the policy are `Arc`s,
/// so each ask assembles a fresh engine without re-constructing them.
pub struct Agent {
    pub name: String,
    pub version: String,
    pub description: String,
    pub traces: Arc<dyn TraceBackend>,
    pub system_prompt: Option<String>,
    /// Definition-owned skill folders. Runtime asks may add more through
    /// `Ask.skills` without mutating the reusable agent.
    pub skill_paths: Vec<PathBuf>,
    pub models: Vec<(ModelSelector, Arc<dyn ModelAdapter>)>,
    pub default_model: Option<ModelSelector>,
    pub capabilities: Vec<Arc<dyn Capability>>,
    pub context_policy: Option<BudgetPolicy>,
    pub clock: Option<Clock>,
    pub scratch: Arc<dyn ScratchBackend>,
    pub scratch_scope: Value,
    pub blobs: Arc<dyn BlobBackend>,
    pub feedback: Arc<dyn FeedbackBackend>,
    /// Per-tier pricing used to derive `AnswerMeta.cost_micro_usd` from trace-recorded usage. Missing tiers price at zero.
    pub pricing: Pricing,
    pub limits: AgentLimits,
    /// Optional typed response contract. When set, the generated JSON Schema is passed to the model provider and the final JSON is cast into the Rust type before it becomes `Answer.response`.
    pub response_contract: Option<ResponseContract>,
    /// Compile-time registered host-side hooks: deterministic Rust wiring owned by the agent crate, not manifest/runtime policy; they run outside `huggr-core` so the reducer stays pure.
    pub ask_hooks: Vec<AskHook>,
    pub answer_hooks: Vec<AnswerHook>,
    /// Granted child agents exposed as ordinary `agent_<name>` capabilities. Registered fresh per ask so each invocation's child cost folds into this ask's `AnswerMeta`.
    pub agent_tools: Vec<AgentToolSpec>,
    fs_trace_store: Option<TraceStore>,
    fs_blob_store: Option<FsBlobStore>,
    fs_feedback_store: Option<FsFeedbackStore>,
}

impl Clone for Agent {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            version: self.version.clone(),
            description: self.description.clone(),
            traces: self.traces.clone(),
            system_prompt: self.system_prompt.clone(),
            skill_paths: self.skill_paths.clone(),
            models: self.models.clone(),
            default_model: self.default_model.clone(),
            capabilities: self.capabilities.clone(),
            context_policy: self.context_policy.clone(),
            clock: self.clock.clone(),
            scratch: self.scratch.clone(),
            scratch_scope: self.scratch_scope.clone(),
            blobs: self.blobs.clone(),
            feedback: self.feedback.clone(),
            pricing: self.pricing.clone(),
            limits: self.limits.clone(),
            response_contract: self.response_contract.clone(),
            ask_hooks: self.ask_hooks.clone(),
            answer_hooks: self.answer_hooks.clone(),
            agent_tools: self.agent_tools.clone(),
            fs_trace_store: self.fs_trace_store.clone(),
            fs_blob_store: self.fs_blob_store.clone(),
            fs_feedback_store: self.fs_feedback_store.clone(),
        }
    }
}

impl Agent {
    /// A fresh agent with defaults. `name`/`version` are stamped into every trace header; `store` is where the immutable traces live. The scratch and blob roots can be overridden through public fields.
    pub fn new(name: impl Into<String>, version: impl Into<String>, store: TraceStore) -> Agent {
        let scratch_root = store.root().join(DEFAULT_SCRATCH_DIRNAME);
        let blob_store = BlobStore::new(store.root().join(DEFAULT_BLOBS_DIRNAME));
        let feedback_store = FsFeedbackStore::new(store.root().join(DEFAULT_FEEDBACK_DIRNAME));
        let storage = StorageOverrides::new(
            Arc::new(store.clone()),
            Arc::new(blob_store.clone()),
            Arc::new(FsScratch::new(&scratch_root)),
        )
        .with_scratch_scope(json!({ "root": scratch_root.display().to_string() }));
        let mut agent = Agent::with_storage(name, version, storage);
        agent.fs_trace_store = Some(store);
        agent.fs_blob_store = Some(blob_store);
        agent.feedback = Arc::new(feedback_store.clone());
        agent.fs_feedback_store = Some(feedback_store);
        agent
    }

    pub fn with_storage(
        name: impl Into<String>,
        version: impl Into<String>,
        storage: StorageOverrides,
    ) -> Agent {
        Agent {
            name: name.into(),
            version: version.into(),
            description: String::new(),
            traces: storage.traces,
            system_prompt: None,
            skill_paths: Vec::new(),
            models: Vec::new(),
            default_model: None,
            capabilities: Vec::new(),
            context_policy: None,
            clock: None,
            scratch: storage.scratch,
            scratch_scope: storage.scratch_scope,
            blobs: storage.blobs,
            feedback: storage.feedback,
            pricing: Pricing::default(),
            limits: AgentLimits::default(),
            response_contract: None,
            ask_hooks: Vec::new(),
            answer_hooks: Vec::new(),
            agent_tools: Vec::new(),
            fs_trace_store: None,
            fs_blob_store: None,
            fs_feedback_store: None,
        }
    }

    /// The trace store this agent persists into.
    pub fn store(&self) -> &TraceStore {
        self.fs_trace_store
            .as_ref()
            .expect("Agent::store is only available on filesystem-backed agents")
    }

    pub fn trace_backend(&self) -> Arc<dyn TraceBackend> {
        self.traces.clone()
    }

    /// The content-addressed blob store this agent's outbound blobs land in. An orchestrator resolves an [`Answer`] blob's `sha256` ref through here.
    pub fn blob_store(&self) -> &BlobStore {
        self.fs_blob_store
            .as_ref()
            .expect("Agent::blob_store is only available on filesystem-backed agents")
    }

    pub fn blob_backend(&self) -> Arc<dyn BlobBackend> {
        self.blobs.clone()
    }

    pub fn set_blob_store(&mut self, store: FsBlobStore) {
        self.blobs = Arc::new(store.clone());
        self.fs_blob_store = Some(store);
    }

    pub fn set_feedback_store(&mut self, store: FsFeedbackStore) {
        self.feedback = Arc::new(store.clone());
        self.fs_feedback_store = Some(store);
    }

    pub async fn feedback(
        &self,
        trace_id: TraceId,
        payload: Value,
    ) -> Result<Feedback, FeedbackError> {
        self.traces.head(&trace_id).await.map_err(|err| match err {
            crate::StoreError::NotFound { .. } => FeedbackError::UnknownTrace(trace_id.clone()),
            other => FeedbackError::Trace(other),
        })?;
        let feedback = Feedback::new(trace_id, payload);
        self.feedback.append(feedback.clone()).await?;
        Ok(feedback)
    }

    pub async fn feedback_for(&self, trace_id: &TraceId) -> Result<Vec<Feedback>, FeedbackError> {
        self.feedback.list(trace_id).await
    }

    pub async fn stats(&self, options: StatsOptions) -> Result<AgentStats, AnalyticsError> {
        collect_stats(
            self.traces.clone(),
            self.feedback.clone(),
            &self.pricing,
            options,
        )
        .await
    }

    /// Describe this agent's public card: identity, tools + privileges, model tiers, pricing, and declared limits.
    pub fn describe(&self) -> AgentCard {
        let mut tools: Vec<ToolCard> = scratch_tool_schemas()
            .into_iter()
            .map(|schema| ToolCard {
                name: schema.name.clone(),
                description: schema.description.clone(),
                privilege: "scratchpad".to_string(),
                runs_in_background: false,
                schema,
                scope: self.scratch_scope.clone(),
            })
            .collect();
        // Granted child agents show as `agent_<name>` tools; they are registered per-ask but are part of the agent's advertised surface.
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
            context: self
                .context_policy
                .as_ref()
                .and_then(|policy| serde_json::to_value(policy).ok())
                .unwrap_or(Value::Null),
            limits: self.limits.clone(),
        }
    }

    /// List stored trace headers for this agent — the same cheap header-only read as [`TraceStore::list`].
    pub async fn traces(&self) -> Result<Vec<TraceHead>, StoreError> {
        self.traces.list().await
    }

    pub async fn traces_with_feedback(&self) -> Result<Vec<TraceListing>, AnalyticsError> {
        list_traces_with_feedback(self.traces.clone(), self.feedback.clone()).await
    }

    /// Run one ask to completion. See the module docs for the fresh-vs-resume split and the error discipline.
    pub async fn ask(&self, ask: Ask) -> Result<Answer, AskError> {
        self.ask_with_frontend(ask, Box::new(SilentFrontend)).await
    }

    pub fn ask_events(
        &self,
        ask: Ask,
    ) -> (
        UnboundedReceiver<AgentEvent>,
        JoinHandle<Result<Answer, AskError>>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = self.clone();
        let handle = tokio::spawn(async move {
            let _ = tx.send(AgentEvent::AskStarted {
                trace_parent: ask.trace_id.clone(),
            });
            let result = agent
                .ask_with_frontend(ask, Box::new(EventFrontend { tx: tx.clone() }))
                .await;
            match &result {
                Ok(answer) => {
                    let _ = tx.send(AgentEvent::AnswerReady {
                        answer: Box::new(answer.clone()),
                    });
                }
                Err(err) => {
                    let _ = tx.send(AgentEvent::Notice {
                        message: err.to_string(),
                    });
                }
            }
            result
        });
        (rx, handle)
    }

    async fn ask_with_frontend(
        &self,
        mut ask: Ask,
        frontend: Box<dyn Frontend>,
    ) -> Result<Answer, AskError> {
        let started = Instant::now();
        for hook in &self.ask_hooks {
            hook.apply(&mut ask);
        }
        let parent = ask.trace_id.clone();

        // Recording is always on: the trace *is* the product here.
        let mut builder = Engine::builder().record(true).frontend(frontend);
        // Counting/cost limits wrap each model adapter so a call over budget is refused (and folded as an ordinary `ModelError`); the wall-clock timeout wraps the turn below. Both surface as an error *answer* with a persisted trace.
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
        let mut skill_paths = self.skill_paths.clone();
        skill_paths.extend(ask.skills.iter().map(PathBuf::from));
        let skills = discover_skills(&skill_paths)?;
        let system = skills_prompt(self.system_prompt.as_deref().unwrap_or(""), &skills);
        if !system.is_empty() {
            builder = builder.system_prompt(system);
        }
        for capability in skills.capabilities() {
            builder = builder.capability(capability);
        }
        if let Some(policy) = &self.context_policy {
            builder = builder.budget_policy(policy.clone());
        }
        if let Some(contract) = &self.response_contract {
            builder = builder.model_request_extra(contract.request_extra());
        }
        if let Some(clock) = &self.clock {
            builder = builder.clock(clock.clone());
        }
        let parent_trace = match &parent {
            Some(parent_id) => Some(self.traces.get(parent_id).await?),
            None => None,
        };

        // Register each granted child agent as an `agent_<name>` capability with a per-ask spend sink, so its cost folds into *this* ask's meta after the turn.
        let child_spend: Arc<Mutex<Vec<AnswerMeta>>> = Arc::new(Mutex::new(Vec::new()));
        for spec in &self.agent_tools {
            builder = builder.capability(Arc::new(AgentTool::new(spec, child_spend.clone())));
        }

        if let Some(trace) = parent_trace {
            // Re-fold the parent's recorded events into the fresh brain — no model or tool is ever re-run for work that already happened; `resume` only rebuilds state.
            builder = builder.resume(trace);
        }

        // A fresh working scratch subtree, seeded by copying the parent's finalized subtree on resume/fork — so this ask sees the ancestor's notes but never a sibling's writes.
        let scratch_handle = self.scratch.prepare(parent.as_ref()).await?;
        let scratch = ScratchSession::new(self.scratch.clone(), scratch_handle);
        for capability in scratch.capabilities() {
            builder = builder.capability(capability);
        }

        // Materialize inbound blobs into the working scratch dir *before* the turn, so tools see plain files in the jail. Malformed hand-ins are infra errors (AskError), not answers.
        blobs::materialize_inbound(&scratch, &ask.blobs, self.blobs.as_ref()).await?;

        let mut engine = builder.build();

        // Accounting baseline: on resume the brain's log already holds the parent's entries; this ask's meta must cover only the new turn (resume never re-bills ancestry).
        let baseline = engine.brain().state().log().len();

        let max_response_attempts = self
            .response_contract
            .as_ref()
            .map(|contract| contract.max_attempts)
            .unwrap_or(1)
            .max(1);
        let mut response_result = None;
        let deadline = self
            .limits
            .timeout_ms
            .filter(|ms| *ms > 0)
            .map(|ms| tokio::time::Instant::now() + std::time::Duration::from_millis(ms));
        for attempt in 1..=max_response_attempts {
            let question = if attempt == 1 {
                ask.question.clone()
            } else {
                response_retry_prompt(response_result.as_ref().unwrap())
            };
            // On timeout the turn future is dropped mid-flight; the recorded event prefix is self-consistent, so the persisted (partial) trace still replays. `session_end` later flushes the final checkpoint.
            match deadline {
                Some(deadline) => {
                    if tokio::time::timeout_at(deadline, engine.user_turn(question))
                        .await
                        .is_err()
                    {
                        engine.abort_and_drain().await;
                        let ms = self
                            .limits
                            .timeout_ms
                            .expect("deadline has a configured timeout");
                        limit_state.record_timeout(ms);
                    }
                }
                _ => engine.user_turn(question).await,
            }
            if limit_state.trip().is_some() {
                break;
            }
            let parsed = final_response(
                &engine.brain().state().log()[baseline..],
                self.response_contract.as_ref(),
            );
            let should_retry = parsed.retryable && attempt < max_response_attempts;
            response_result = Some(parsed);
            if !should_retry {
                break;
            }
        }
        engine.session_end();

        // A limit trip supersedes the log-derived answer: it is an error answer with a typed reason on `extra`. Otherwise the final model output is the answer.
        let trip = limit_state.trip();
        let (status, response, extra) = match &trip {
            Some(trip) => (
                STATUS_ERROR.to_string(),
                error_response(trip.message(), Value::Null),
                trip.extra(),
            ),
            None => {
                let response = response_result.unwrap_or_else(|| {
                    final_response(
                        &engine.brain().state().log()[baseline..],
                        self.response_contract.as_ref(),
                    )
                });
                (response.status, response.value, Value::Null)
            }
        };

        // Persist old + new as one NEW immutable trace; the parent file is never mutated — lineage lives in `depends_on`.
        let trace = engine
            .trace()
            .expect("recording is always enabled on an agent engine");
        let mut metadata = meta_from_trace(
            &trace,
            baseline,
            started.elapsed().as_millis() as u64,
            &self.pricing,
        );
        // Fold each delegated child agent's spend into this ask's meta.
        for child_meta in child_spend.lock().unwrap().iter() {
            metadata.merge_child(child_meta);
        }
        let mut header = TraceHeader::new(&self.name, &self.version, &ask.question, &status)
            .with_extra(ask.extra.clone());
        if let Some(parent_id) = parent {
            header = header.with_depends_on(parent_id);
        }
        let trace_id = self.traces.put(trace, header).await?;

        // Sweep the `out/` scratch subtree into the content-addressed store and return the files as outbound blobs. Done before finalize while the working subtree is still in place.
        let out_blobs = blobs::sweep_outbound(&scratch, self.blobs.as_ref()).await?;

        // Finalize the working subtree under the new trace's id so a later resume/fork of *this* trace can seed from it. Scratch is never recorded, so this move happens after the trace is persisted.
        self.scratch.finalize(scratch.handle(), &trace_id).await?;

        let mut answer = Answer {
            status,
            response,
            trace_id,
            blobs: out_blobs,
            metadata,
            extra,
        };
        for hook in &self.answer_hooks {
            hook.apply(&mut answer);
        }
        Ok(answer)
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

/// A compile-time registered hook that can adjust an [`Ask`] before the agent builds the turn. Hooks live in the host layer, never in `huggr-core`.
#[derive(Clone)]
pub struct AskHook {
    name: String,
    apply: Arc<dyn Fn(&mut Ask) + Send + Sync>,
}

impl std::fmt::Debug for AskHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskHook")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl AskHook {
    pub fn new(name: impl Into<String>, apply: impl Fn(&mut Ask) + Send + Sync + 'static) -> Self {
        Self {
            name: name.into(),
            apply: Arc::new(apply),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn apply(&self, ask: &mut Ask) {
        (self.apply)(ask);
    }
}

/// A compile-time registered hook that can adjust the final [`Answer`] at the very end of an ask, after the trace/scratch/blob work is complete and just before the surface returns to the caller.
#[derive(Clone)]
pub struct AnswerHook {
    name: String,
    apply: Arc<dyn Fn(&mut Answer) + Send + Sync>,
}

impl std::fmt::Debug for AnswerHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnswerHook")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl AnswerHook {
    pub fn new(
        name: impl Into<String>,
        apply: impl Fn(&mut Answer) + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            apply: Arc::new(apply),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn apply(&self, answer: &mut Answer) {
        (self.apply)(answer);
    }
}

/// Public description returned by [`Agent::describe`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentCard {
    pub name: String,
    pub version: String,
    pub description: String,
    pub tools: Vec<ToolCard>,
    pub model_tiers: Vec<ModelTierCard>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context: Value,
    pub limits: AgentLimits,
}

/// One advertised tool plus the privilege metadata surfaces need.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCard {
    pub name: String,
    pub description: String,
    /// Coarse privilege label (`read_only` / `scratchpad` / `gated` / `agent`) — an open string set nothing branches on.
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

/// Declared runtime limits, enforced host-side on every ask and exposed on the introspection card. Each `None` field is unbounded.
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

/// Typed response contract for one agent. The schema is sent to model providers as an opaque request knob; the parser only casts returned JSON into the declared Rust type, it does not perform independent JSON Schema validation.
#[derive(Clone)]
pub struct ResponseContract {
    pub name: String,
    pub schema: Value,
    pub public_schema: Value,
    pub max_attempts: u8,
    parse: Arc<dyn Fn(Value) -> Result<Value, String> + Send + Sync>,
}

impl std::fmt::Debug for ResponseContract {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResponseContract")
            .field("name", &self.name)
            .field("schema", &self.schema)
            .field("public_schema", &self.public_schema)
            .field("max_attempts", &self.max_attempts)
            .finish_non_exhaustive()
    }
}

impl ResponseContract {
    pub fn from_type<T>(name: impl Into<String>) -> Self
    where
        T: DeserializeOwned + Serialize + JsonSchema + 'static,
    {
        let schema = serde_json::to_value(schema_for!(T)).expect("response schema serializes");
        Self::with_parser(name, schema, |value| {
            let typed: T = serde_json::from_value(value).map_err(|err| err.to_string())?;
            serde_json::to_value(typed).map_err(|err| err.to_string())
        })
    }

    pub fn from_schema(name: impl Into<String>, schema: Value) -> Self {
        Self::with_parser(name, schema, |value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err("final response JSON must be an object".to_string())
            }
        })
    }

    pub fn with_max_attempts(mut self, max_attempts: u8) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    fn with_parser<F>(name: impl Into<String>, schema: Value, parse: F) -> Self
    where
        F: Fn(Value) -> Result<Value, String> + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            public_schema: schema.clone(),
            schema,
            max_attempts: 3,
            parse: Arc::new(parse),
        }
    }

    pub fn with_public_type<T>(mut self) -> Self
    where
        T: JsonSchema + 'static,
    {
        self.public_schema =
            serde_json::to_value(schema_for!(T)).expect("public response schema serializes");
        self
    }

    pub fn public_schema(&self) -> &Value {
        &self.public_schema
    }

    fn request_extra(&self) -> Value {
        json!({
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": self.name,
                    "strict": true,
                    "schema": self.schema,
                },
            },
        })
    }

    fn cast(&self, value: Value) -> Result<Value, String> {
        (self.parse)(value)
    }
}

/// Per-tier token prices used by [`Agent`] cost accounting. Values are USD per million tokens, matching provider price sheets. The computed answer cost is rounded to the nearest micro-USD.
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

/// Infrastructure failures of an ask — everything that prevents a trace from being persisted at all. Run failures are *answers*, not errors.
#[derive(Debug, thiserror::Error)]
pub enum AskError {
    /// The parent trace could not be loaded, or the new trace could not be persisted.
    #[error(transparent)]
    Store(#[from] StoreError),

    /// The per-lineage scratchpad subtree could not be prepared, seeded, or finalized on disk.
    #[error("scratchpad IO error: {0}")]
    Scratch(#[from] std::io::Error),

    /// An inbound blob could not be materialized, or an outbound blob could not be swept into the content-addressed store.
    #[error("blob exchange error: {0}")]
    Blob(#[from] BlobError),

    /// A definition-owned or runtime skill folder could not be discovered or validated.
    #[error(transparent)]
    Skill(#[from] crate::skills::SkillError),
}

/// The parent id an [`AskError::Store`] not-found refers to, if any — a small convenience for surfaces mapping infra errors to error answers.
impl AskError {
    pub fn missing_trace(&self) -> Option<&TraceId> {
        match self {
            AskError::Store(StoreError::NotFound { id }) => Some(id),
            _ => None,
        }
    }
}

struct FinalResponse {
    status: String,
    value: Value,
    retryable: bool,
}

/// Extract the final response from the durable log: the last model output with
/// no tool calls is the turn's answer. No text means the run failed before
/// answering: an error *answer* with the terminal error surfaced.
fn final_response(log: &[LogEntry], contract: Option<&ResponseContract>) -> FinalResponse {
    let final_text = log.iter().rev().find_map(|entry| match &entry.record {
        Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
            Some(output.text.clone())
        }
        _ => None,
    });
    match final_text {
        Some(text) => match parse_model_response(&text, contract) {
            Ok(response) => FinalResponse {
                status: STATUS_SUCCESS.to_string(),
                value: response,
                retryable: false,
            },
            Err(error) => FinalResponse {
                status: STATUS_ERROR.to_string(),
                value: error_response(
                    format!("model response did not match the response contract: {error}"),
                    json!({ "raw": text }),
                ),
                retryable: true,
            },
        },
        None => FinalResponse {
            status: STATUS_ERROR.to_string(),
            value: error_response(missing_final_answer_message(log), Value::Null),
            retryable: false,
        },
    }
}

fn parse_model_response(text: &str, contract: Option<&ResponseContract>) -> Result<Value, String> {
    let trimmed = strip_json_fence(text.trim());
    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) if value.is_object() => match contract {
            Some(contract) => contract.cast(value),
            None => Ok(value),
        },
        Ok(_) => Err("final response JSON must be an object".to_string()),
        Err(_) if contract.is_none() => Ok(json!({ "text": text.trim() })),
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

fn response_retry_prompt(last: &FinalResponse) -> String {
    let error = last
        .value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("the previous response could not be parsed");
    format!(
        "Your previous response could not be accepted: {error}. Return only the structured response requested by the provider response schema."
    )
}

fn missing_final_answer_message(log: &[LogEntry]) -> String {
    let terminal_error = log.iter().rev().find_map(|entry| match &entry.record {
        Record::OpEnded {
            outcome: huggr_core::OpOutcome::Error(error),
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

/// A no-op front-end: a huglet's product is its `Answer` + trace, not a
/// terminal render. Surfaces that want live output can grow a builder knob
/// later without touching the contract.
struct SilentFrontend;

impl Frontend for SilentFrontend {}

struct EventFrontend {
    tx: UnboundedSender<AgentEvent>,
}

impl EventFrontend {
    fn send(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

impl Frontend for EventFrontend {
    fn on_output(&mut self, event: &OutputEvent) {
        match event {
            OutputEvent::ModelText { op, text } => self.send(AgentEvent::TextDelta {
                op: *op,
                text: text.clone(),
            }),
            OutputEvent::Notice(message) => self.send(AgentEvent::Notice {
                message: message.clone(),
            }),
            _ => {}
        }
    }

    fn on_notice(&mut self, message: &str) {
        self.send(AgentEvent::Notice {
            message: message.to_string(),
        });
    }

    fn on_model_start(&mut self, op: OpId, selector: &ModelSelector) {
        self.send(AgentEvent::ModelStarted {
            op,
            tier: selector.0.clone(),
        });
    }

    fn on_model_end(&mut self, op: OpId, usage: &Usage) {
        self.send(AgentEvent::ModelEnded {
            op,
            usage: usage.clone(),
        });
    }

    fn on_tool_start(&mut self, op: OpId, name: &str, args: &Value) {
        self.send(AgentEvent::ToolStarted {
            op,
            name: name.to_string(),
            args: args.clone(),
        });
    }

    fn on_tool_end(&mut self, op: OpId, name: &str, result: &Value, is_error: bool) {
        self.send(AgentEvent::ToolEnded {
            op,
            name: name.to_string(),
            is_error,
            result: result.clone(),
        });
    }

    fn on_done(&mut self, reason: &DoneReason) {
        self.send(AgentEvent::Done {
            reason: reason.clone(),
        });
    }
}
