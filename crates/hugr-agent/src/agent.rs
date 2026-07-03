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

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use hugr_core::{LogEntry, ModelSelector, OpId, Record, SamplingParams};
use hugr_host::policy::AllowAll;
use hugr_host::{Capability, Clock, Engine, Frontend, ModelAdapter, Policy};
use hugr_replay::{BlobStore, Trace};

use crate::blobs::{self, BlobError};
use crate::contract::{Answer, AnswerMeta, AnswerStatus, Ask, TraceId};
use crate::scratch::{ScratchDir, copy_tree};
use crate::store::{StoreError, TraceHeader, TraceStore};

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
/// any stored trace. Build one with [`Agent::builder`].
///
/// Cheap to share pieces: adapters, capabilities, and the policy are `Arc`s,
/// so each ask assembles a fresh engine without re-constructing them.
#[non_exhaustive]
pub struct Agent {
    name: String,
    version: String,
    store: TraceStore,
    system_prompt: Option<String>,
    models: Vec<(ModelSelector, Arc<dyn ModelAdapter>)>,
    default_model: Option<ModelSelector>,
    capabilities: Vec<Arc<dyn Capability>>,
    policy: Arc<dyn Policy>,
    sampling: Option<SamplingParams>,
    clock: Option<Clock>,
    /// Root of the per-lineage scratchpad subtree (ARCHITECTURE §19.3).
    scratch_root: PathBuf,
    /// Content-addressed store outbound blobs land in (ARCHITECTURE §18.3).
    blob_store: BlobStore,
    /// Per-tier pricing used to derive `AnswerMeta.cost_micro_usd` from
    /// trace-recorded usage (ARCHITECTURE §18.4). Missing tiers price at zero.
    pricing: Pricing,
    /// Monotonic counter naming each ask's pending working directory — the one
    /// piece of host-side nondeterminism, kept off the trace (scratch content
    /// never enters the log; results carry only relative paths).
    next_scratch: Arc<AtomicU64>,
}

impl Agent {
    /// Start building an agent. `name`/`version` are stamped into every trace
    /// header; `store` is where the immutable traces live.
    pub fn builder(
        name: impl Into<String>,
        version: impl Into<String>,
        store: TraceStore,
    ) -> AgentBuilder {
        AgentBuilder {
            name: name.into(),
            version: version.into(),
            store,
            system_prompt: None,
            models: Vec::new(),
            default_model: None,
            capabilities: Vec::new(),
            policy: None,
            sampling: None,
            clock: None,
            scratch_root: None,
            blob_store: None,
            pricing: Pricing::default(),
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

    /// Run one ask to completion (ARCHITECTURE §18.1/§19.2). See the module
    /// docs for the fresh-vs-resume split and the error discipline.
    pub async fn ask(&self, ask: Ask) -> Result<Answer, AskError> {
        let started = Instant::now();
        let parent = ask.trace_id.clone();

        // Assemble a fresh engine per ask. Recording is always on: the trace
        // *is* the product here.
        let mut builder = Engine::builder()
            .record(true)
            .policy(self.policy.clone())
            .frontend(Box::new(SilentFrontend));
        for (selector, adapter) in &self.models {
            builder = builder.model(selector.clone(), adapter.clone());
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
        if let Some(parent_id) = &parent {
            // Load the parent (one file read) and re-fold its recorded events
            // into the fresh brain — no model or tool is ever re-run for work
            // that already happened (§15.1); `resume` only rebuilds state.
            let trace = self.store.get(parent_id)?;
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

        engine.user_turn(ask.question.clone()).await;
        engine.session_end();

        let log = engine.brain().state().log();
        let (status, message) = final_answer(log);
        // Persist old + new as one NEW immutable trace; the parent file is
        // never mutated — lineage lives in `depends_on` (§19.2).
        let trace = engine
            .trace()
            .expect("recording is always enabled on an agent engine");
        let metadata = meta_from_trace(
            &trace,
            baseline,
            started.elapsed().as_millis() as u64,
            &self.pricing,
        );
        let mut header = TraceHeader::new(
            &self.name,
            &self.version,
            &ask.question,
            status_wire(status),
        );
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

        Ok(Answer::new(status, message, trace_id, metadata).with_blobs(out_blobs))
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
}

/// Builds an [`Agent`]. Mirrors `hugr_host::EngineBuilder` for the pieces an
/// agent definition declares; everything not set gets the host default.
#[non_exhaustive]
pub struct AgentBuilder {
    name: String,
    version: String,
    store: TraceStore,
    system_prompt: Option<String>,
    models: Vec<(ModelSelector, Arc<dyn ModelAdapter>)>,
    default_model: Option<ModelSelector>,
    capabilities: Vec<Arc<dyn Capability>>,
    policy: Option<Arc<dyn Policy>>,
    sampling: Option<SamplingParams>,
    clock: Option<Clock>,
    scratch_root: Option<PathBuf>,
    blob_store: Option<BlobStore>,
    pricing: Pricing,
}

impl AgentBuilder {
    /// Register a model adapter under a logical selector. The first registered
    /// selector is the default unless [`default_model`](Self::default_model)
    /// overrides it (same rule as the engine builder).
    pub fn model(mut self, selector: ModelSelector, adapter: Arc<dyn ModelAdapter>) -> Self {
        self.models.push((selector, adapter));
        self
    }

    /// Override which logical selector the turn policy calls.
    pub fn default_model(mut self, selector: ModelSelector) -> Self {
        self.default_model = Some(selector);
        self
    }

    /// Grant a capability (tool). Sandbox-by-registration (§18.2): only what
    /// is registered here exists for the agent — never register more than the
    /// definition grants.
    pub fn capability(mut self, capability: Arc<dyn Capability>) -> Self {
        self.capabilities.push(capability);
        self
    }

    /// Set the system prompt.
    pub fn system_prompt(mut self, system: impl Into<String>) -> Self {
        self.system_prompt = Some(system.into());
        self
    }

    /// Set the host permission policy (default: `AllowAll` — appropriate for
    /// pre-vetted, jailed tool sets).
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set sampling parameters for every model request.
    pub fn sampling(mut self, sampling: SamplingParams) -> Self {
        self.sampling = Some(sampling);
        self
    }

    /// Override the host clock (tests inject a deterministic counter so
    /// recorded traces are reproducible).
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Override the per-lineage scratchpad root (ARCHITECTURE §19.3). Defaults
    /// to a hidden `.scratch` subtree inside the trace store root, so a store
    /// dir carries its agents' scratch lineage alongside the traces.
    pub fn scratch_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.scratch_root = Some(root.into());
        self
    }

    /// Override the content-addressed blob store outbound blobs land in
    /// (ARCHITECTURE §18.3). Defaults to a hidden `.blobs` subtree inside the
    /// trace store root, so a store dir carries its agents' blobs alongside the
    /// traces.
    pub fn blob_store(mut self, store: BlobStore) -> Self {
        self.blob_store = Some(store);
        self
    }

    /// Set per-tier pricing for cost accounting (ARCHITECTURE §18.4). The
    /// trace records selector + token usage; this host-side table turns those
    /// durable facts into `AnswerMeta.cost_micro_usd`.
    pub fn pricing(mut self, pricing: Pricing) -> Self {
        self.pricing = pricing;
        self
    }

    pub fn build(self) -> Agent {
        let scratch_root = self
            .scratch_root
            .unwrap_or_else(|| self.store.root().join(DEFAULT_SCRATCH_DIRNAME));
        let blob_store = self
            .blob_store
            .unwrap_or_else(|| BlobStore::new(self.store.root().join(DEFAULT_BLOBS_DIRNAME)));
        Agent {
            name: self.name,
            version: self.version,
            store: self.store,
            system_prompt: self.system_prompt,
            models: self.models,
            default_model: self.default_model,
            capabilities: self.capabilities,
            policy: self.policy.unwrap_or_else(|| Arc::new(AllowAll)),
            sampling: self.sampling,
            clock: self.clock,
            scratch_root,
            blob_store,
            pricing: self.pricing,
            next_scratch: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Per-tier token prices used by [`Agent`] cost accounting (ROADMAP T0.6).
/// Values are USD per million tokens, matching provider price sheets. The
/// computed answer cost is rounded to the nearest micro-USD.
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
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

    fn cost_micro_usd(&self, selector: &str, input_tokens: u64, output_tokens: u64) -> u64 {
        let Some(price) = self.tiers.get(selector) else {
            return 0;
        };
        let cost = (input_tokens as f64 * price.input_usd_per_m_tokens)
            + (output_tokens as f64 * price.output_usd_per_m_tokens);
        cost.round().max(0.0) as u64
    }
}

/// One tier's input/output prices in USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
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
#[non_exhaustive]
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

/// Extract the final answer from the durable log: the last model output with
/// no tool calls is the turn's answer. No text means the run failed before
/// answering — an error *answer* (§18.1), with the terminal error surfaced.
/// Off-topic classification is agent-specific and lives above this layer
/// (`Answer.extra` / the docs port, T0.8).
fn final_answer(log: &[LogEntry]) -> (AnswerStatus, String) {
    let final_text = log.iter().rev().find_map(|entry| match &entry.record {
        Record::ModelOutput { output, .. } if output.tool_calls.is_empty() => {
            Some(output.text.clone())
        }
        _ => None,
    });
    match final_text {
        Some(text) => (AnswerStatus::Success, text),
        None => (
            AnswerStatus::Error,
            missing_final_answer_message(log).to_string(),
        ),
    }
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
/// resumed ask never re-bills its ancestry). Recorded child traces tied to new
/// agent ops are folded recursively, so sub-agent cost rolls up when present.
fn meta_from_trace(
    trace: &Trace,
    baseline: usize,
    duration_ms: u64,
    pricing: &Pricing,
) -> AnswerMeta {
    let new_entries = &trace.log[baseline..];
    let mut aggregate = SpendAggregate::default();
    aggregate_log(new_entries, pricing, &mut aggregate);

    let child_ops: BTreeSet<OpId> = new_entries
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::OpEnded { op, meta, .. } if meta.model.is_none() && meta.usage.is_none() => {
                Some(*op)
            }
            _ => None,
        })
        .collect();
    for child in &trace.children {
        if child_ops.contains(&child.op) {
            let child_baseline = child.seed.len().min(child.trace.log.len());
            aggregate_trace(&child.trace, child_baseline, pricing, &mut aggregate);
        }
    }

    aggregate.into_meta(duration_ms)
}

fn aggregate_trace(
    trace: &Trace,
    baseline: usize,
    pricing: &Pricing,
    aggregate: &mut SpendAggregate,
) {
    let baseline = baseline.min(trace.log.len());
    let new_entries = &trace.log[baseline..];
    aggregate_log(new_entries, pricing, aggregate);

    let child_ops: BTreeSet<OpId> = new_entries
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::OpEnded { op, meta, .. } if meta.model.is_none() && meta.usage.is_none() => {
                Some(*op)
            }
            _ => None,
        })
        .collect();
    for child in &trace.children {
        if child_ops.contains(&child.op) {
            let child_baseline = child.seed.len().min(child.trace.log.len());
            aggregate_trace(&child.trace, child_baseline, pricing, aggregate);
        }
    }
}

fn aggregate_log(new_entries: &[LogEntry], pricing: &Pricing, aggregate: &mut SpendAggregate) {
    let mut tool_calls = 0u32;
    for entry in new_entries {
        let Record::OpEnded { meta, .. } = &entry.record else {
            continue;
        };
        if let (Some(selector), Some(usage)) = (&meta.model, &meta.usage) {
            let selector = selector_name(selector);
            let tier = aggregate
                .tiers
                .entry(selector.clone())
                .or_insert_with(|| TierAccumulator::new(selector));
            tier.model_calls += 1;
            tier.tokens_in += usage.input_tokens;
            tier.tokens_out += usage.output_tokens;
            tier.cost_micro_usd +=
                pricing.cost_micro_usd(&tier.selector, usage.input_tokens, usage.output_tokens);
        } else if meta.model.is_none() {
            tool_calls += 1;
        }
    }
    aggregate.tool_calls += tool_calls;
}

fn selector_name(selector: &ModelSelector) -> String {
    match selector {
        ModelSelector::Named(name) => name.clone(),
        #[allow(unreachable_patterns)]
        _ => format!("{selector:?}"),
    }
}

#[derive(Default)]
struct SpendAggregate {
    tiers: BTreeMap<String, TierAccumulator>,
    tool_calls: u32,
}

impl SpendAggregate {
    fn into_meta(self, duration_ms: u64) -> AnswerMeta {
        let mut meta = AnswerMeta::new()
            .with_duration_ms(duration_ms)
            .with_tool_calls(self.tool_calls);
        for tier in self.tiers.into_values() {
            meta = meta.with_tier(crate::contract::TierSpend::new(
                tier.selector,
                tier.model_calls,
                tier.tokens_in,
                tier.tokens_out,
                tier.cost_micro_usd,
            ));
        }
        meta
    }
}

struct TierAccumulator {
    selector: String,
    model_calls: u32,
    tokens_in: u64,
    tokens_out: u64,
    cost_micro_usd: u64,
}

impl TierAccumulator {
    fn new(selector: String) -> Self {
        Self {
            selector,
            model_calls: 0,
            tokens_in: 0,
            tokens_out: 0,
            cost_micro_usd: 0,
        }
    }
}

/// The wire string of an [`AnswerStatus`] as stamped into trace headers —
/// matches the contract's serde `snake_case` form.
fn status_wire(status: AnswerStatus) -> &'static str {
    match status {
        AnswerStatus::Success => "success",
        AnswerStatus::OffTopic => "off_topic",
        AnswerStatus::Error => "error",
        // `AnswerStatus` is #[non_exhaustive]; new variants must add a wire
        // string here alongside the contract change.
        #[allow(unreachable_patterns)]
        _ => "error",
    }
}

/// A no-op front-end: a subagent's product is its `Answer` + trace, not a
/// terminal render. Surfaces that want live output can grow a builder knob
/// later without touching the contract.
struct SilentFrontend;

impl Frontend for SilentFrontend {}
