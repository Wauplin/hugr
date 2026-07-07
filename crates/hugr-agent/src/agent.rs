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
use crate::contract::{
    Access, Answer, AnswerMeta, AnswerStatus, Ask, ResourceGrant, ResourceGroup, ResourceRef,
    TraceId,
};
use crate::limits::{LimitState, LimitedAdapter};
use crate::scratch::{ScratchDir, copy_tree, scratch_tool_schemas};
use crate::store::{
    PrunePolicy, PruneReport, StoreError, StoreSize, TraceHead, TraceHeader, TraceStore,
};

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
    /// Optional JSON schema for `Answer.extra` (ROADMAP T3.4). When set, the
    /// agent lifts the final JSON message into `extra` and validates it against
    /// this schema post-hoc — violations become `Answer.warnings`, never errors.
    pub answer_schema: Option<Value>,
    /// Effective config with real provenance, supplied by the layer that knows
    /// where values came from (the toolkit's `build_agent`: manifest/env/flag,
    /// secrets redacted — ROADMAP T3.5). When `None`, [`Agent::config`] derives a
    /// builder/default-tagged view from its own fields.
    pub config_entries: Option<Vec<ConfigEntry>>,
    /// Manifest tools bound to a resource group (`group:<name>`, ARCHITECTURE
    /// §18.5, ROADMAP T3.7). Each is registered only for asks that carry a
    /// grant of sufficient access over its group.
    pub group_bindings: Vec<GroupBinding>,
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
            answer_schema: None,
            config_entries: None,
            group_bindings: Vec::new(),
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
                privilege: ToolPrivilege::Scratchpad,
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
                privilege: ToolPrivilege::Agent,
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
                    ToolPrivilege::Gated
                } else {
                    ToolPrivilege::ReadOnly
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

    /// Return the effective runtime configuration that built this agent, with
    /// stable provenance and redaction slots for future manifest/env/flag
    /// sources (ARCHITECTURE §18.2).
    pub fn config(&self) -> AgentConfig {
        // The toolkit supplies provenance-annotated entries (manifest/env/flag,
        // secrets redacted — T3.5). Absent that, derive a builder/default view.
        if let Some(entries) = &self.config_entries {
            return AgentConfig {
                entries: entries.clone(),
            };
        }
        let mut entries = vec![
            ConfigEntry::visible("agent.name", self.name.clone(), ConfigProvenance::Builder),
            ConfigEntry::visible(
                "agent.version",
                self.version.clone(),
                ConfigProvenance::Builder,
            ),
            ConfigEntry::visible(
                "agent.description",
                self.description.clone(),
                if self.description.is_empty() {
                    ConfigProvenance::Default
                } else {
                    ConfigProvenance::Builder
                },
            ),
            ConfigEntry::visible(
                "traces.store_root",
                self.store.root().display().to_string(),
                ConfigProvenance::Builder,
            ),
            ConfigEntry::visible(
                "scratchpad.root",
                self.scratch_root.display().to_string(),
                ConfigProvenance::Builder,
            ),
            ConfigEntry::visible(
                "models.default",
                self.default_model
                    .as_ref()
                    .map(selector_name)
                    .unwrap_or_else(|| {
                        self.models
                            .first()
                            .map(|(selector, _)| selector_name(selector))
                            .unwrap_or_else(|| "medium".to_string())
                    }),
                if self.default_model.is_some() {
                    ConfigProvenance::Builder
                } else {
                    ConfigProvenance::Default
                },
            ),
            ConfigEntry::visible(
                "limits",
                serde_json::to_value(&self.limits).expect("limits serialize"),
                if self.limits == AgentLimits::default() {
                    ConfigProvenance::Default
                } else {
                    ConfigProvenance::Builder
                },
            ),
        ];

        for tier in self.model_tiers() {
            entries.push(ConfigEntry::visible(
                format!("models.{}", tier.selector),
                json!({
                    "selector": tier.selector,
                    "default": tier.default,
                    "pricing": tier.pricing,
                }),
                ConfigProvenance::Builder,
            ));
        }
        for tool in &self.describe().tools {
            entries.push(ConfigEntry::visible(
                format!("tools.{}", tool.name),
                json!({
                    "privilege": tool.privilege,
                    "runs_in_background": tool.runs_in_background,
                    "scope": tool.scope.clone(),
                }),
                ConfigProvenance::Builder,
            ));
        }

        AgentConfig { entries }
    }

    /// List stored trace headers for this agent. This is the same cheap
    /// header-only read as [`TraceStore::list`].
    pub fn traces(&self) -> Result<Vec<TraceHead>, StoreError> {
        self.store.list()
    }

    /// Prune stored traces under `policy` and delete the pruned traces'
    /// per-lineage scratch subtrees so scratch state does not outlive its trace
    /// (ROADMAP T3.3). Lineage closure is enforced by the store, so a surviving
    /// trace's `depends_on` chain always still resolves. Blob-store GC is a
    /// separate concern (blobs are content-addressed and shared across traces).
    pub fn prune(&self, policy: &PrunePolicy) -> Result<PruneReport, StoreError> {
        let report = self.store.prune(policy)?;
        for id in &report.pruned {
            let scratch = self.scratch_root.join(id.as_str());
            if scratch.exists() {
                std::fs::remove_dir_all(&scratch)?;
            }
        }
        Ok(report)
    }

    /// The store's on-disk size (trace count + bytes), for lifecycle reporting.
    pub fn store_size(&self) -> Result<StoreSize, StoreError> {
        self.store.size()
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
        // Load the parent (one file read) up front so we can both re-fold it and
        // read its recorded grants (§18.5) before it moves into `resume`.
        let parent_trace = match &parent {
            Some(parent_id) => Some(self.store.get(parent_id)?),
            None => None,
        };

        // Resource groups & grants (ARCHITECTURE §18.5, ROADMAP T3.7): the
        // effective grants are this ask's if it supplies any, else the parent's
        // recorded ones — so a resume with no new grants re-derives the identical
        // registration from the trace alone. Each group-bound tool registers only
        // when a grant of sufficient access over its group is present.
        let mut warnings: Vec<String> = Vec::new();
        let (eff_groups, eff_grants) = effective_grants(&ask, parent_trace.as_ref());
        for capability in self.resolve_group_bindings(&eff_groups, &eff_grants, &mut warnings) {
            builder = builder.capability(capability);
        }

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
        let (status, message, mut extra) = match &trip {
            Some(trip) => (AnswerStatus::Error, trip.message(), trip.extra()),
            None => {
                let (status, message) = final_answer(log);
                (status, message, Value::Null)
            }
        };

        // Structured answer extras (ROADMAP T3.4): when a schema is declared and
        // the run produced a plain (non-error, no-limit-trip) answer, lift the
        // final JSON message into `extra` and validate it. Violations are
        // advisory warnings on the answer, never failures — `extra` is never
        // load-bearing for the contract. (`warnings` already carries any
        // group-binding notices from registration; schema notices append.)
        if let Some(schema) = &self.answer_schema {
            if trip.is_none() && extra.is_null() {
                if let Ok(parsed) = serde_json::from_str::<Value>(message.trim()) {
                    if parsed.is_object() || parsed.is_array() {
                        extra = parsed;
                    }
                }
            }
            if !extra.is_null() {
                warnings.extend(crate::answer_schema::validate_extra(schema, &extra));
            }
        }
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
        let mut header = TraceHeader::new(
            &self.name,
            &self.version,
            &ask.question,
            status_wire(status),
        );
        if let Some(parent_id) = parent {
            header = header.with_depends_on(parent_id);
        }
        // Record the effective groups + grants (§18.5) so a resume/fork
        // re-derives the identical registration from the trace alone.
        if !eff_groups.is_empty() || !eff_grants.is_empty() {
            header = header.with_grants(json!({
                "groups": eff_groups,
                "grants": eff_grants,
            }));
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

        let mut answer = Answer::new(status, message, trace_id, metadata).with_blobs(out_blobs);
        if !extra.is_null() {
            answer = answer.with_extra(extra);
        }
        if !warnings.is_empty() {
            answer = answer.with_warnings(warnings);
        }
        Ok(answer)
    }

    /// Resolve this agent's group bindings against the effective groups/grants
    /// (§18.5): for each binding, register its capabilities only when a grant of
    /// sufficient access over its group is present and the group is offered.
    /// A grant with insufficient access, or a factory error, is a `warnings`
    /// notice (advisory) — a simply-absent grant is silent (the normal
    /// no-access case). Deterministic order: bindings in registration order.
    fn resolve_group_bindings(
        &self,
        groups: &[ResourceGroup],
        grants: &[ResourceGrant],
        warnings: &mut Vec<String>,
    ) -> Vec<Arc<dyn Capability>> {
        let mut caps = Vec::new();
        for binding in &self.group_bindings {
            let Some(grant) = grants.iter().find(|g| g.group == binding.group) else {
                // No grant for this group → tool is not registered (by design).
                continue;
            };
            if !grant.access.satisfies(binding.required_access) {
                warnings.push(format!(
                    "tool `{}` bound to group `{}` requires {:?} access but the grant is {:?}; not registered",
                    binding.tool, binding.group, binding.required_access, grant.access
                ));
                continue;
            }
            let Some(group) = groups.iter().find(|g| g.name == binding.group) else {
                warnings.push(format!(
                    "group `{}` is granted but not offered on the ask; tool `{}` not registered",
                    binding.group, binding.tool
                ));
                continue;
            };
            match (binding.factory)(&group.resources) {
                Ok(built) => caps.extend(built),
                Err(err) => warnings.push(format!(
                    "tool `{}` bound to group `{}` failed to build: {err}; not registered",
                    binding.tool, binding.group
                )),
            }
        }
        caps
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

/// Builds the capabilities a group-bound tool registers from a granted group's
/// resources (ARCHITECTURE §18.5, ROADMAP T3.7). Returns an error string when
/// the group's resources are unusable for this tool (e.g. no `FsRoot`); a
/// binding that errors is skipped with a warning rather than failing the ask.
pub type GroupCapabilityFactory =
    Arc<dyn Fn(&[ResourceRef]) -> Result<Vec<Arc<dyn Capability>>, String> + Send + Sync>;

/// A manifest tool bound to a resource group (`group:<name>`). Registered only
/// when an ask carries a [`ResourceGrant`](crate::ResourceGrant) of at least
/// `required_access` over `group`.
#[non_exhaustive]
pub struct GroupBinding {
    /// The group name this tool binds to.
    pub group: String,
    /// The bound tool's id, for diagnostics (e.g. `"fs_read"`).
    pub tool: String,
    /// Minimum granted access for this tool to register.
    pub required_access: Access,
    /// Builds the tool's capabilities from the granted group's resources.
    pub factory: GroupCapabilityFactory,
}

impl GroupBinding {
    pub fn new(
        group: impl Into<String>,
        tool: impl Into<String>,
        required_access: Access,
        factory: GroupCapabilityFactory,
    ) -> Self {
        Self {
            group: group.into(),
            tool: tool.into(),
            required_access,
            factory,
        }
    }
}

/// Public description returned by [`Agent::describe`] (ROADMAP T0.7).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
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
#[non_exhaustive]
pub struct ToolCard {
    pub name: String,
    pub description: String,
    pub privilege: ToolPrivilege,
    pub runs_in_background: bool,
    pub schema: ToolSchema,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub scope: Value,
}

/// Coarse privilege class. T1's manifest tool library will refine scopes; this
/// T0 layer reports what the registered capability can tell us today.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolPrivilege {
    ReadOnly,
    Scratchpad,
    Gated,
    /// A granted child agent exposed as a tool (§20.5, T3.8).
    Agent,
}

/// One logical model tier exposed in the agent card.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelTierCard {
    pub selector: String,
    pub default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<TierPrice>,
}

/// Declared runtime limits, enforced host-side on every ask (ROADMAP T3.1) and
/// exposed on the T0.7 introspection card. Each `None` field is unbounded.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
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

    pub fn with_max_turns(mut self, max_turns: u32) -> Self {
        self.max_turns = Some(max_turns);
        self
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

/// Effective configuration with value provenance and redaction.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AgentConfig {
    pub entries: Vec<ConfigEntry>,
}

/// One effective configuration value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ConfigEntry {
    pub key: String,
    pub value: Value,
    pub provenance: ConfigProvenance,
    pub redacted: bool,
}

impl ConfigEntry {
    pub fn visible(
        key: impl Into<String>,
        value: impl Serialize,
        provenance: ConfigProvenance,
    ) -> Self {
        Self {
            key: key.into(),
            value: serde_json::to_value(value).expect("config values serialize"),
            provenance,
            redacted: false,
        }
    }

    pub fn redacted(key: impl Into<String>, provenance: ConfigProvenance) -> Self {
        Self {
            key: key.into(),
            value: Value::String("<redacted>".to_string()),
            provenance,
            redacted: true,
        }
    }
}

/// Where an effective configuration value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigProvenance {
    Default,
    Builder,
    Manifest,
    Env,
    Flag,
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

/// The groups + grants recorded in a trace header (§18.5), for re-derivation on
/// resume/fork. Serialized into the opaque `TraceMeta.grants` slot.
#[derive(Default, Serialize, Deserialize)]
struct RecordedGrants {
    #[serde(default)]
    groups: Vec<ResourceGroup>,
    #[serde(default)]
    grants: Vec<ResourceGrant>,
}

/// The effective resource groups + grants for an ask (§18.5): the ask's own if
/// it supplies any (groups or grants), else the parent trace's recorded set —
/// so a plain resume re-derives the identical registration from the trace
/// alone, while a follow-up that changes grants records the new fact.
fn effective_grants(
    ask: &Ask,
    parent_trace: Option<&Trace>,
) -> (Vec<ResourceGroup>, Vec<ResourceGrant>) {
    if !ask.groups.is_empty() || !ask.grants.is_empty() {
        return (ask.groups.clone(), ask.grants.clone());
    }
    if let Some(value) = parent_trace.and_then(|t| t.meta.grants.as_ref()) {
        if let Ok(recorded) = serde_json::from_value::<RecordedGrants>(value.clone()) {
            return (recorded.groups, recorded.grants);
        }
    }
    (Vec::new(), Vec::new())
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

    aggregate.into_meta(duration_ms)
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
    selector.0.clone()
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
