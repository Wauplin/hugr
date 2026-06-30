//! The pluggable turn strategy.
//!
//! `TurnPolicy` is the **only place agent strategy lives** (ARCHITECTURE §2.5).
//! The reducer asks it which model to call, how to project context from the
//! log, and whether a capability needs permission — but never hardcodes those
//! decisions. Swap the policy to change behaviour without touching the reducer.

use serde_json::json;

use crate::model::{
    ContentPart, ContextBlock, ModelRequest, ModelSelector, Role, SamplingParams, ToolSchema,
};
use crate::record::{LogEntry, Record};
use crate::state::BrainState;

/// Strategy for driving the turn loop. Implementations must be **pure**:
/// [`project_context`](TurnPolicy::project_context) only *reads* the log (no IO,
/// no model calls — compaction is a separate model op, ARCHITECTURE §3.4).
pub trait TurnPolicy {
    /// Pick which logical model to call for the next step (multi-model routing).
    fn choose_model(&self, state: &BrainState) -> ModelSelector;

    /// Render the model context from the log. Pure and synchronous: include /
    /// summarize / evict-to-reference / drop. Must never call a model.
    fn project_context(&self, log: &[LogEntry]) -> ModelRequest;

    /// Whether invoking `capability` requires a permission round-trip.
    fn needs_permission(&self, capability: &str) -> bool;
}

/// A simple, configurable [`TurnPolicy`] with a **trivial pass-through
/// projection**: it renders the log into context blocks one-to-one, with no
/// summarization or eviction. This is the Phase 0 policy (ROADMAP Phase 0).
///
/// It is also genuinely useful as a default and as a test fixture: the model
/// selector, the advertised tool schemas, and the set of permissioned
/// capabilities are all configurable.
#[derive(Clone, Debug)]
pub struct StaticPolicy {
    model: ModelSelector,
    tools: Vec<ToolSchema>,
    permissioned: Vec<String>,
    params: SamplingParams,
}

impl Default for StaticPolicy {
    fn default() -> Self {
        Self {
            model: ModelSelector::named("big"),
            tools: Vec::new(),
            permissioned: Vec::new(),
            params: SamplingParams::default(),
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

    /// Set sampling parameters applied to every request.
    pub fn with_params(mut self, params: SamplingParams) -> Self {
        self.params = params;
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
                Record::ToolResult { op, result, .. } => {
                    blocks.push(ContextBlock::new(
                        Role::Tool,
                        vec![ContentPart::ToolResult {
                            id: op.to_string(),
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
}
