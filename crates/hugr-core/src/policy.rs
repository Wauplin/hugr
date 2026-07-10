//! The pluggable turn strategy.
//!
//! `TurnPolicy` is the only place agent strategy lives.
//! The reducer asks it which model to call, how to project context from the
//! log, and whether a capability needs permission — but never hardcodes those
//! decisions. Swap the policy to change behaviour without touching the reducer.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::model::{
    ContentPart, ContextBlock, ContextBudgetTotals, ContextDisposition, ContextPlan,
    ContextPlanEntry, ContextSource, ModelSelector, Role, SamplingParams, TokenBudget, ToolSchema,
};
use crate::primitives::Value;
use crate::record::{LogEntry, Record};
use crate::state::BrainState;

/// Decode a policy captured as an opaque [`Value`] — e.g. a trace's stored
/// policy config. Returns `None` when the value does not decode as a
/// [`StaticPolicy`] (e.g. a custom host policy); the caller picks its own
/// fallback. Faithful replay needs the *same* policy a session was recorded
/// under, because the brain branches on its pure decisions.
pub fn decode_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    serde_json::from_value::<StaticPolicy>(value.clone())
        .ok()
        .map(|policy| Box::new(policy) as Box<dyn TurnPolicy>)
}

/// Strategy for driving the turn loop. Implementations must be **pure**:
/// [`project_context`](TurnPolicy::project_context) only *reads* the log (no
/// IO, no model calls).
///
/// `Send + Sync` so the host may move the whole brain onto a worker task; the
/// brain itself is still reduced single-threaded.
pub trait TurnPolicy: Send + Sync {
    /// Pick which logical model to call for the next step. Pure: derive the
    /// choice only from `state`.
    fn choose_model(&self, state: &BrainState) -> ModelSelector;

    /// Pick the token budget the next context projection plans against.
    fn context_budget(&self, _state: &BrainState) -> TokenBudget {
        TokenBudget::default()
    }

    /// Plan the model context from the log. Pure and synchronous: include /
    /// summarize / evict-to-reference / drop. Must never call a model.
    fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan;

    /// Whether invoking `capability` requires a permission round-trip.
    fn needs_permission(&self, capability: &str) -> bool;

    /// Whether `capability` runs in the **background**: it does not block the
    /// model turn, so the model keeps streaming while the op runs. Its result
    /// is folded into the log when it finishes and picked up at the next turn
    /// boundary. Defaults to `false` (foreground: the turn waits for it).
    fn is_background(&self, _capability: &str) -> bool {
        false
    }
}

/// A simple, configurable [`TurnPolicy`] with a **trivial pass-through
/// projection**: it renders the log into context blocks one-to-one, with no
/// summarization or eviction.
///
/// It is also genuinely useful as a default and as a test fixture: the model
/// selector, the advertised tool schemas, and the set of permissioned
/// capabilities are all configurable.
///
/// It is `Serialize`/`Deserialize` so a host can persist a session's policy
/// alongside its trace (the pure branching — `needs_permission`,
/// `is_background`, advertised tools, model selector — must be reproduced for
/// bit-for-bit replay.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StaticPolicy {
    model: ModelSelector,
    tools: Vec<ToolSchema>,
    permissioned: Vec<String>,
    background: Vec<String>,
    params: SamplingParams,
    system: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    extra: Value,
    #[serde(default)]
    context_budget: TokenBudget,
}

impl Default for StaticPolicy {
    fn default() -> Self {
        Self {
            model: ModelSelector::named("medium"),
            tools: Vec::new(),
            permissioned: Vec::new(),
            background: Vec::new(),
            params: SamplingParams::default(),
            system: None,
            extra: Value::Null,
            context_budget: TokenBudget::default(),
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
    /// turn, so the model keeps streaming while they run.
    pub fn with_background(mut self, names: impl IntoIterator<Item = String>) -> Self {
        self.background = names.into_iter().collect();
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

    /// Set provider-specific request extras applied to every model call.
    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }

    /// Set the approximate input token budget used by context planning.
    pub fn with_context_budget(mut self, budget: TokenBudget) -> Self {
        self.context_budget = budget;
        self
    }
}

impl TurnPolicy for StaticPolicy {
    fn choose_model(&self, _state: &BrainState) -> ModelSelector {
        self.model.clone()
    }

    fn context_budget(&self, _state: &BrainState) -> TokenBudget {
        self.context_budget
    }

    fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan {
        // One context block per logged message / result, in log order.
        let mut entries = Vec::new();
        let mut totals = ContextBudgetTotals::new();
        // One projected block: count it against the budget totals and record
        // the plan entry, in one step. The arms that deliberately do *not*
        // count against the totals (`OpEnded` bookkeeping) push their entries
        // directly instead of calling this.
        fn push(
            totals: &mut ContextBudgetTotals,
            entries: &mut Vec<ContextPlanEntry>,
            source: ContextSource,
            est_tokens: u32,
            disposition: ContextDisposition,
        ) {
            totals.add(&disposition, est_tokens);
            entries.push(ContextPlanEntry::new(source, est_tokens, disposition));
        }
        if let Some(system) = &self.system {
            let disposition = ContextDisposition::included(ContextBlock::new(
                Role::System,
                vec![ContentPart::Text(system.clone())],
            ));
            push(
                &mut totals,
                &mut entries,
                ContextSource::system(),
                0,
                disposition,
            );
        }
        let mut projected_tool_results = HashSet::new();
        for entry in log {
            if projected_tool_results.contains(&entry.seq) {
                let est_tokens = entry.record.content_est_tokens().unwrap_or(0);
                entries.push(ContextPlanEntry::new(
                    ContextSource::log_entry(entry.seq),
                    est_tokens,
                    ContextDisposition::omitted(),
                ));
                continue;
            }
            match &entry.record {
                Record::UserMessage {
                    text, est_tokens, ..
                } => {
                    let disposition = ContextDisposition::included(ContextBlock::new(
                        Role::User,
                        vec![ContentPart::Text(text.clone())],
                    ));
                    push(
                        &mut totals,
                        &mut entries,
                        ContextSource::log_entry(entry.seq),
                        *est_tokens,
                        disposition,
                    );
                }
                Record::ModelOutput {
                    output, est_tokens, ..
                } => {
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
                        let disposition =
                            ContextDisposition::included(ContextBlock::new(Role::Assistant, parts));
                        push(
                            &mut totals,
                            &mut entries,
                            ContextSource::log_entry(entry.seq),
                            *est_tokens,
                            disposition,
                        );
                    }
                    // OpenAI-compatible chat formats require tool result
                    // messages to immediately follow the assistant message
                    // containing the corresponding `tool_calls`. Durable host
                    // hooks and op metadata can be logged between those facts,
                    // so projection groups matching results here without
                    // changing the append-only log.
                    for call in &output.tool_calls {
                        if let Some(result_entry) =
                            find_tool_result_for_call(log, entry.seq, &call.id)
                        {
                            if let Record::ToolResult {
                                call_id,
                                result,
                                est_tokens,
                                ..
                            } = &result_entry.record
                            {
                                let disposition = ContextDisposition::included(ContextBlock::new(
                                    Role::Tool,
                                    vec![ContentPart::ToolResult {
                                        id: call_id.clone(),
                                        result: result.clone(),
                                    }],
                                ));
                                push(
                                    &mut totals,
                                    &mut entries,
                                    ContextSource::log_entry(result_entry.seq),
                                    *est_tokens,
                                    disposition,
                                );
                                projected_tool_results.insert(result_entry.seq);
                            }
                        }
                    }
                }
                Record::ToolResult {
                    call_id,
                    result,
                    est_tokens,
                    ..
                } => {
                    let disposition = ContextDisposition::included(ContextBlock::new(
                        Role::Tool,
                        vec![ContentPart::ToolResult {
                            id: call_id.clone(),
                            result: result.clone(),
                        }],
                    ));
                    push(
                        &mut totals,
                        &mut entries,
                        ContextSource::log_entry(entry.seq),
                        *est_tokens,
                        disposition,
                    );
                }
                // OpEnded entries are bookkeeping (timing/cost); they do not
                // contribute to model context, but the plan still explains why
                // the block is omitted.
                Record::OpEnded { .. } => {
                    let disposition = ContextDisposition::omitted();
                    entries.push(ContextPlanEntry::new(
                        ContextSource::log_entry(entry.seq),
                        0,
                        disposition,
                    ));
                }
            }
        }

        ContextPlan::new(
            budget,
            entries,
            totals,
            self.tools.clone(),
            self.params.clone(),
        )
        .with_extra(self.extra.clone())
    }

    fn needs_permission(&self, capability: &str) -> bool {
        self.permissioned.iter().any(|c| c == capability)
    }

    fn is_background(&self, capability: &str) -> bool {
        self.background.iter().any(|c| c == capability)
    }
}

fn find_tool_result_for_call<'a>(
    log: &'a [LogEntry],
    after: crate::primitives::Seq,
    call_id: &str,
) -> Option<&'a LogEntry> {
    log.iter().find(|entry| {
        entry.seq > after
            && matches!(
                &entry.record,
                Record::ToolResult {
                    call_id: result_call_id,
                    ..
                } if result_call_id == call_id
            )
    })
}
