//! The pluggable turn strategy.
//!
//! `TurnPolicy` is the **only place agent strategy lives** (ARCHITECTURE §2.5).
//! The reducer asks it which model to call, how to project context from the
//! log, and whether a capability needs permission — but never hardcodes those
//! decisions. Swap the policy to change behaviour without touching the reducer.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    ContentPart, ContextBlock, ModelRequest, ModelSelector, Role, SamplingParams, ToolSchema,
};
use crate::record::{LogEntry, Record};
use crate::state::BrainState;

/// How to seed a **sub-agent's** log when it is spawned (ARCHITECTURE §14). A
/// fork is *copying a log prefix*: the child then evolves independently. Values
/// are compared, never parsed — the brain resolves this to the actual prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AgentSeed {
    /// Empty log — a fully isolated child (no shared context).
    Fresh,
    /// Copy the parent's log entries up to and including this `seq` — shared
    /// context, then diverge (also the primitive under branch/rewind).
    ForkAt { seq: u64 },
    /// Copy the entire current parent log — shared full context.
    ForkFull,
}

/// Strategy for driving the turn loop. Implementations must be **pure**:
/// [`project_context`](TurnPolicy::project_context) only *reads* the log (no IO,
/// no model calls — compaction is a separate model op, ARCHITECTURE §3.4).
///
/// `Send + Sync` so the host may move the whole brain onto a worker task — the
/// brain is still reduced single-threaded (CLAUDE.md); this only lets a host
/// (e.g. a sub-agent runner, ARCHITECTURE §13.2) own a brain on another thread.
pub trait TurnPolicy: Send + Sync {
    /// Pick which logical model to call for the next step (multi-model routing).
    fn choose_model(&self, state: &BrainState) -> ModelSelector;

    /// Render the model context from the log. Pure and synchronous: include /
    /// summarize / evict-to-reference / drop. Must never call a model.
    fn project_context(&self, log: &[LogEntry]) -> ModelRequest;

    /// Whether invoking `capability` requires a permission round-trip.
    fn needs_permission(&self, capability: &str) -> bool;

    /// Whether `capability` runs in the **background**: it does not block the
    /// model turn, so the model keeps streaming while the op runs (ARCHITECTURE
    /// §6.3 — "a long `cargo build` and a model response concurrently"). Its
    /// result is folded into the log when it finishes and picked up at the next
    /// turn boundary. Defaults to `false` (foreground: the turn waits for it).
    fn is_background(&self, _capability: &str) -> bool {
        false
    }

    /// Whether invoking `capability` spawns a **sub-agent** rather than an
    /// ordinary capability, and if so how to seed the child's log — fork the
    /// parent's context ([`ForkFull`](AgentSeed::ForkFull) /
    /// [`ForkAt`](AgentSeed::ForkAt)) or start [`Fresh`](AgentSeed::Fresh)
    /// (ARCHITECTURE §13/§14). `None` (the default) means an ordinary
    /// capability. This is *strategy*, so it lives here, not in the reducer:
    /// the brain merely emits [`Command::StartAgent`](crate::Command::StartAgent)
    /// instead of `StartCapability` when this returns `Some`.
    fn agent_seed(&self, _capability: &str) -> Option<AgentSeed> {
        None
    }
}

/// A simple, configurable [`TurnPolicy`] with a **trivial pass-through
/// projection**: it renders the log into context blocks one-to-one, with no
/// summarization or eviction. This is the Phase 0 policy (ROADMAP Phase 0).
///
/// It is also genuinely useful as a default and as a test fixture: the model
/// selector, the advertised tool schemas, and the set of permissioned
/// capabilities are all configurable.
///
/// It is `Serialize`/`Deserialize` so a host can persist a session's policy
/// alongside its trace (the pure branching — `needs_permission`,
/// `is_background`, advertised tools, model selector — must be reproduced for
/// bit-for-bit replay, ARCHITECTURE §6.3).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticPolicy {
    model: ModelSelector,
    tools: Vec<ToolSchema>,
    permissioned: Vec<String>,
    background: Vec<String>,
    /// Capability names that spawn a sub-agent, each with its seed strategy.
    /// `#[serde(default)]` so traces recorded before Phase 6 still decode.
    #[serde(default)]
    agents: Vec<(String, AgentSeed)>,
    params: SamplingParams,
    system: Option<String>,
}

impl Default for StaticPolicy {
    fn default() -> Self {
        Self {
            model: ModelSelector::named("big"),
            tools: Vec::new(),
            permissioned: Vec::new(),
            background: Vec::new(),
            agents: Vec::new(),
            params: SamplingParams::default(),
            system: None,
        }
    }
}

impl StaticPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the logical model every turn uses.
    pub fn with_model(mut self, model: ModelSelector) -> Self {
        self.model = model;
        self
    }

    /// Advertise these tool schemas to the model each turn.
    pub fn with_tools(mut self, tools: Vec<ToolSchema>) -> Self {
        self.tools = tools;
        self
    }

    /// Require a permission round-trip before invoking any of these capabilities.
    pub fn with_permissioned(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.permissioned = names.into_iter().collect();
        self
    }

    /// Run these capabilities in the background: they do not block the model
    /// turn, so the model keeps streaming while they run (ARCHITECTURE §6.3).
    pub fn with_background(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.background = names.into_iter().collect();
        self
    }

    /// Treat `name` as a **sub-agent spawner**: invoking it starts a child brain
    /// seeded per `seed` rather than an ordinary capability (ARCHITECTURE §13/§14).
    pub fn with_agent(mut self, name: impl Into<String>, seed: AgentSeed) -> Self {
        self.agents.push((name.into(), seed));
        self
    }

    /// Treat each of these capability names as a sub-agent spawner, sharing the
    /// parent's full context ([`AgentSeed::ForkFull`]). Use
    /// [`with_agent`](Self::with_agent) for a different seed strategy.
    pub fn with_agents(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.agents
            .extend(names.into_iter().map(|n| (n, AgentSeed::ForkFull)));
        self
    }

    /// Set sampling parameters applied to every request.
    pub fn with_params(mut self, params: SamplingParams) -> Self {
        self.params = params;
        self
    }

    /// Set the system prompt prepended to every projected request.
    pub fn with_system_prompt(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }
}

impl TurnPolicy for StaticPolicy {
    fn choose_model(&self, _state: &BrainState) -> ModelSelector {
        self.model.clone()
    }

    fn project_context(&self, log: &[LogEntry]) -> ModelRequest {
        // Trivial pass-through: one context block per logged message / result,
        // in log order. No compaction, no eviction (those arrive later).
        let mut blocks = Vec::new();
        if let Some(system) = &self.system {
            blocks.push(ContextBlock::new(
                Role::System,
                vec![ContentPart::Text(system.clone())],
            ));
        }
        for entry in log {
            match &entry.record {
                Record::UserMessage { text } => {
                    blocks.push(ContextBlock::new(
                        Role::User,
                        vec![ContentPart::Text(text.clone())],
                    ));
                }
                Record::ModelOutput { output, .. } => {
                    let mut parts = Vec::new();
                    if !output.text.is_empty() {
                        parts.push(ContentPart::Text(output.text.clone()));
                    }
                    for call in &output.tool_calls {
                        parts.push(ContentPart::ToolUse {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: call.args.clone(),
                        });
                    }
                    if !parts.is_empty() {
                        blocks.push(ContextBlock::new(Role::Assistant, parts));
                    }
                }
                Record::ToolResult {
                    call_id, result, ..
                } => {
                    blocks.push(ContextBlock::new(
                        Role::Tool,
                        vec![ContentPart::ToolResult {
                            id: call_id.clone(),
                            result: result.clone(),
                        }],
                    ));
                }
                // OpEnded entries are bookkeeping (timing/cost); they do not
                // contribute to model context.
                Record::OpEnded { .. } => {}
            }
        }

        ModelRequest {
            blocks,
            tools: self.tools.clone(),
            params: self.params.clone(),
            extra: json!(null),
        }
    }

    fn needs_permission(&self, capability: &str) -> bool {
        self.permissioned.iter().any(|c| c == capability)
    }

    fn is_background(&self, capability: &str) -> bool {
        self.background.iter().any(|c| c == capability)
    }

    fn agent_seed(&self, capability: &str) -> Option<AgentSeed> {
        self.agents
            .iter()
            .find(|(name, _)| name == capability)
            .map(|(_, seed)| *seed)
    }
}
