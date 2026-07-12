//! The pluggable turn strategy.
//!
//! `TurnPolicy` is the only place agent strategy lives.
//! The reducer asks it which model to call, how to project context from the
//! log, and whether a capability needs permission — but never hardcodes those
//! decisions. Swap the policy to change behaviour without touching the reducer.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::model::{
    ContentPart, ContextBlock, ContextBudgetTotals, ContextDisposition, ContextPlan,
    ContextPlanEntry, ContextSource, ModelRequest, ModelSelector, Role, SummaryRequest,
    TokenBudget, ToolSchema,
};
use crate::primitives::Value;
use crate::record::{LogEntry, Record};
use crate::state::BrainState;

pub type PolicyDecoder = fn(&Value) -> Option<Box<dyn TurnPolicy>>;

#[derive(Clone)]
pub struct PolicyRegistry {
    decoders: BTreeMap<String, PolicyDecoder>,
}

impl PolicyRegistry {
    pub fn new() -> Self {
        Self {
            decoders: BTreeMap::new(),
        }
    }

    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register("static", decode_static_policy);
        registry.register("budget", decode_budget_policy);
        registry
    }

    pub fn register(&mut self, kind: impl Into<String>, decoder: PolicyDecoder) {
        self.decoders.insert(kind.into(), decoder);
    }

    pub fn decode(&self, value: &Value) -> Option<Box<dyn TurnPolicy>> {
        match value.get("kind").and_then(Value::as_str) {
            Some(kind) => self.decoders.get(kind).and_then(|decode| decode(value)),
            None => decode_static_policy(value),
        }
    }
}

impl Default for PolicyRegistry {
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl std::fmt::Debug for PolicyRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PolicyRegistry")
            .field("kinds", &self.decoders.keys().collect::<Vec<_>>())
            .finish()
    }
}

/// Decode a policy captured as an opaque [`Value`] — e.g. a trace's stored
/// policy config. Returns `None` when the value does not decode through the
/// built-in registry; the caller picks its own fallback. Faithful replay needs
/// the *same* policy a session was recorded under, because the brain branches
/// on its pure decisions.
pub fn decode_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    PolicyRegistry::default().decode(value)
}

fn decode_static_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    serde_json::from_value::<StaticPolicy>(value.clone())
        .ok()
        .map(|policy| Box::new(policy) as Box<dyn TurnPolicy>)
}

fn decode_budget_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    serde_json::from_value::<BudgetPolicy>(value.clone())
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
    #[serde(default = "static_policy_kind")]
    kind: String,
    model: ModelSelector,
    tools: Vec<ToolSchema>,
    permissioned: Vec<String>,
    background: Vec<String>,
    system: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    extra: Value,
    #[serde(default)]
    context_budget: TokenBudget,
}

fn static_policy_kind() -> String {
    "static".to_string()
}

impl Default for StaticPolicy {
    fn default() -> Self {
        Self {
            kind: static_policy_kind(),
            model: ModelSelector::named("medium"),
            tools: Vec::new(),
            permissioned: Vec::new(),
            background: Vec::new(),
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
        let active_summary = active_summary(log);
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
            if let Some((_, replaces_up_to)) = active_summary
                && entry.seq <= replaces_up_to
            {
                entries.push(ContextPlanEntry::new(
                    ContextSource::log_entry(entry.seq),
                    entry.record.content_est_tokens().unwrap_or(0),
                    ContextDisposition::omitted(),
                ));
                continue;
            }
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
                Record::ContextSummary {
                    replaces_up_to,
                    text,
                    est_tokens,
                    ..
                } => {
                    if Some((entry.seq, *replaces_up_to)) == active_summary {
                        let disposition = ContextDisposition::included(ContextBlock::new(
                            Role::System,
                            vec![ContentPart::Text(format!(
                                "Context summary through log seq {}:\n{text}",
                                replaces_up_to.0
                            ))],
                        ));
                        push(
                            &mut totals,
                            &mut entries,
                            ContextSource::log_entry(entry.seq),
                            *est_tokens,
                            disposition,
                        );
                    } else {
                        entries.push(ContextPlanEntry::new(
                            ContextSource::log_entry(entry.seq),
                            *est_tokens,
                            ContextDisposition::omitted(),
                        ));
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

        ContextPlan::new(budget, entries, totals, self.tools.clone()).with_extra(self.extra.clone())
    }

    fn needs_permission(&self, capability: &str) -> bool {
        self.permissioned.iter().any(|c| c == capability)
    }

    fn is_background(&self, capability: &str) -> bool {
        self.background.iter().any(|c| c == capability)
    }
}

fn active_summary(log: &[LogEntry]) -> Option<(crate::primitives::Seq, crate::primitives::Seq)> {
    log.iter()
        .filter_map(|entry| match &entry.record {
            Record::ContextSummary { replaces_up_to, .. } => Some((entry.seq, *replaces_up_to)),
            _ => None,
        })
        .max_by_key(|(_, replaces_up_to)| *replaces_up_to)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BudgetPolicy {
    #[serde(default = "budget_policy_kind")]
    kind: String,
    #[serde(default)]
    base: StaticPolicy,
    budget_tokens: u32,
    trigger_tokens: u32,
    keep_recent_tokens: u32,
    max_block_tokens: u32,
    #[serde(default)]
    tool_ttl: BTreeMap<String, u32>,
    #[serde(default)]
    keep_last_per_tool: BTreeMap<String, u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    summary_selector: Option<ModelSelector>,
}

fn budget_policy_kind() -> String {
    "budget".to_string()
}

impl BudgetPolicy {
    pub fn new(budget_tokens: u32) -> Self {
        let budget_tokens = budget_tokens.max(1);
        Self {
            kind: budget_policy_kind(),
            base: StaticPolicy::default()
                .with_context_budget(TokenBudget::new(budget_tokens.into())),
            budget_tokens,
            trigger_tokens: budget_tokens,
            keep_recent_tokens: budget_tokens / 3,
            max_block_tokens: (budget_tokens / 4).max(1),
            tool_ttl: BTreeMap::new(),
            keep_last_per_tool: BTreeMap::new(),
            summary_selector: None,
        }
    }

    pub fn with_base(mut self, base: StaticPolicy) -> Self {
        self.base = base.with_context_budget(TokenBudget::new(self.budget_tokens.into()));
        self
    }

    pub fn with_trigger_tokens(mut self, trigger_tokens: u32) -> Self {
        self.trigger_tokens = trigger_tokens.max(1);
        self
    }

    pub fn with_keep_recent_tokens(mut self, keep_recent_tokens: u32) -> Self {
        self.keep_recent_tokens = keep_recent_tokens;
        self
    }

    pub fn with_max_block_tokens(mut self, max_block_tokens: u32) -> Self {
        self.max_block_tokens = max_block_tokens.max(1);
        self
    }

    pub fn with_tool_ttl(mut self, tool_ttl: BTreeMap<String, u32>) -> Self {
        self.tool_ttl = tool_ttl
            .into_iter()
            .filter(|(name, ttl)| !name.trim().is_empty() && *ttl > 0)
            .collect();
        self
    }

    pub fn with_keep_last_per_tool(mut self, keep_last_per_tool: BTreeMap<String, u32>) -> Self {
        self.keep_last_per_tool = keep_last_per_tool
            .into_iter()
            .filter(|(name, keep)| !name.trim().is_empty() && *keep > 0)
            .collect();
        self
    }

    pub fn with_summary_selector(mut self, selector: ModelSelector) -> Self {
        self.summary_selector = Some(selector);
        self
    }
}

impl TurnPolicy for BudgetPolicy {
    fn choose_model(&self, state: &BrainState) -> ModelSelector {
        self.base.choose_model(state)
    }

    fn context_budget(&self, _state: &BrainState) -> TokenBudget {
        TokenBudget::new(self.budget_tokens.into())
    }

    fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan {
        let mut plan = self.base.project_context(log, budget);
        if plan.totals.used_tokens <= u64::from(self.trigger_tokens) {
            apply_forget_rules(
                log,
                &mut plan.entries,
                &self.tool_ttl,
                &self.keep_last_per_tool,
            );
            plan.totals = totals_for(&plan.entries);
            return plan;
        }

        apply_forget_rules(
            log,
            &mut plan.entries,
            &self.tool_ttl,
            &self.keep_last_per_tool,
        );
        plan.totals = totals_for(&plan.entries);
        let recent = recent_indices(&plan.entries, self.keep_recent_tokens);
        if let Some(selector) = &self.summary_selector
            && let Some(up_to) = summary_cutoff(&plan.entries, &recent)
            && !has_summary_covering(log, up_to)
        {
            plan.wants_summary = Some(summary_request(selector.clone(), up_to, &plan));
            return plan;
        }
        let mut used = plan.totals.used_tokens;
        let target = u64::from(self.budget_tokens);
        let mut dropped = 0u64;
        let mut truncated = 0u64;

        for (idx, entry) in plan.entries.iter_mut().enumerate() {
            if used <= target {
                break;
            }
            if recent.contains(&idx) || matches!(entry.source, ContextSource::System) {
                continue;
            }
            let est = u64::from(entry.est_tokens);
            let disposition =
                std::mem::replace(&mut entry.disposition, ContextDisposition::Omitted);
            match disposition {
                ContextDisposition::Included { block }
                | ContextDisposition::Truncated { block } => {
                    if entry.est_tokens > self.max_block_tokens {
                        let block = truncate_block(block, self.max_block_tokens);
                        used = used.saturating_sub(est) + u64::from(self.max_block_tokens);
                        truncated += est.saturating_sub(u64::from(self.max_block_tokens));
                        entry.est_tokens = self.max_block_tokens;
                        entry.disposition = ContextDisposition::truncated(block);
                    } else {
                        used = used.saturating_sub(est);
                        dropped += est;
                        entry.disposition = ContextDisposition::dropped(Some(
                            "dropped by deterministic budget policy".to_string(),
                        ));
                    }
                }
                other => {
                    entry.disposition = other;
                }
            }
        }

        if used > target {
            for (idx, entry) in plan.entries.iter_mut().enumerate() {
                if used <= target {
                    break;
                }
                if recent.contains(&idx) || matches!(entry.source, ContextSource::System) {
                    continue;
                }
                if matches!(
                    entry.disposition,
                    ContextDisposition::Included { .. } | ContextDisposition::Truncated { .. }
                ) {
                    let est = u64::from(entry.est_tokens);
                    used = used.saturating_sub(est);
                    dropped += est;
                    entry.disposition = ContextDisposition::dropped(Some(
                        "dropped by deterministic budget policy".to_string(),
                    ));
                }
            }
        }

        if dropped > 0 || truncated > 0 {
            let note = format!(
                "Context compacted deterministically: dropped approximately {dropped} token(s), truncated approximately {truncated} token(s)."
            );
            let note_entry = ContextPlanEntry::new(
                ContextSource::system(),
                0,
                ContextDisposition::included(ContextBlock::new(
                    Role::System,
                    vec![ContentPart::Text(note)],
                )),
            );
            let insert_at = plan
                .entries
                .iter()
                .position(|entry| !matches!(entry.source, ContextSource::System))
                .unwrap_or(plan.entries.len());
            plan.entries.insert(insert_at, note_entry);
        }
        balance_tool_call_groups(&mut plan.entries);
        plan.totals = totals_for(&plan.entries);
        plan
    }

    fn needs_permission(&self, capability: &str) -> bool {
        self.base.needs_permission(capability)
    }

    fn is_background(&self, capability: &str) -> bool {
        self.base.is_background(capability)
    }
}

fn apply_forget_rules(
    log: &[LogEntry],
    entries: &mut [ContextPlanEntry],
    tool_ttl: &BTreeMap<String, u32>,
    keep_last_per_tool: &BTreeMap<String, u32>,
) {
    if tool_ttl.is_empty() && keep_last_per_tool.is_empty() {
        return;
    }

    let mut newer_turns = 0u32;
    let mut newer_by_tool: BTreeMap<String, u32> = BTreeMap::new();
    let mut forgotten = HashSet::new();
    for entry in log.iter().rev() {
        match &entry.record {
            Record::UserMessage { .. } => newer_turns = newer_turns.saturating_add(1),
            Record::ToolResult { name, .. } => {
                let newer_same = newer_by_tool.get(name).copied().unwrap_or(0);
                let expired_by_ttl = tool_ttl.get(name).is_some_and(|ttl| newer_turns >= *ttl);
                let expired_by_keep_last = keep_last_per_tool
                    .get(name)
                    .is_some_and(|keep| newer_same >= *keep);
                if expired_by_ttl || expired_by_keep_last {
                    forgotten.insert(entry.seq);
                }
                newer_by_tool.insert(name.clone(), newer_same.saturating_add(1));
            }
            Record::ModelOutput { .. } | Record::ContextSummary { .. } | Record::OpEnded { .. } => {
            }
        }
    }

    for entry in &mut *entries {
        if let ContextSource::LogEntry { seq } = entry.source
            && forgotten.contains(&seq)
        {
            entry.disposition = ContextDisposition::dropped(Some(
                "dropped by deterministic forget rule".to_string(),
            ));
        }
    }
    balance_tool_call_groups(entries);
}

fn balance_tool_call_groups(entries: &mut [ContextPlanEntry]) {
    loop {
        let included_tool_uses = entries
            .iter()
            .flat_map(|entry| match &entry.disposition {
                ContextDisposition::Included { block }
                | ContextDisposition::Truncated { block } => block
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::ToolUse { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                ContextDisposition::Dropped { .. } | ContextDisposition::Omitted => Vec::new(),
            })
            .collect::<HashSet<_>>();
        let included_tool_results = entries
            .iter()
            .flat_map(|entry| match &entry.disposition {
                ContextDisposition::Included { block }
                | ContextDisposition::Truncated { block } => block
                    .content
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::ToolResult { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>(),
                ContextDisposition::Dropped { .. } | ContextDisposition::Omitted => Vec::new(),
            })
            .collect::<HashSet<_>>();

        let mut changed = false;
        for entry in entries.iter_mut() {
            let should_drop = match &entry.disposition {
                ContextDisposition::Included { block }
                | ContextDisposition::Truncated { block } => {
                    block.content.iter().any(|part| match part {
                        ContentPart::ToolUse { id, .. } => !included_tool_results.contains(id),
                        ContentPart::ToolResult { id, .. } => !included_tool_uses.contains(id),
                        ContentPart::Text(_) => false,
                    })
                }
                ContextDisposition::Dropped { .. } | ContextDisposition::Omitted => false,
            };
            if should_drop {
                entry.disposition = ContextDisposition::dropped(Some(
                    "dropped with paired tool transcript block".to_string(),
                ));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn summary_cutoff(
    entries: &[ContextPlanEntry],
    recent: &HashSet<usize>,
) -> Option<crate::primitives::Seq> {
    let mut cutoff = None;
    for (idx, entry) in entries.iter().enumerate() {
        if recent.contains(&idx) {
            break;
        }
        if let ContextSource::LogEntry { seq } = entry.source {
            cutoff = Some(seq);
        }
    }
    cutoff
}

fn has_summary_covering(log: &[LogEntry], up_to: crate::primitives::Seq) -> bool {
    log.iter().any(|entry| {
        matches!(
            &entry.record,
            Record::ContextSummary { replaces_up_to, .. } if *replaces_up_to >= up_to
        )
    })
}

fn summary_request(
    selector: ModelSelector,
    replaces_up_to: crate::primitives::Seq,
    plan: &ContextPlan,
) -> SummaryRequest {
    let mut blocks = vec![ContextBlock::new(
        Role::System,
        vec![ContentPart::Text(
            "Summarize the following conversation context for future turns. Preserve user goals, decisions, constraints, tool findings, and unresolved work. Return only the summary text.".to_string(),
        )],
    )];
    for entry in &plan.entries {
        match entry.source {
            ContextSource::LogEntry { seq } if seq <= replaces_up_to => match &entry.disposition {
                ContextDisposition::Included { block }
                | ContextDisposition::Truncated { block } => {
                    blocks.push(block.clone());
                }
                ContextDisposition::Dropped { .. } | ContextDisposition::Omitted => {}
            },
            ContextSource::System | ContextSource::LogEntry { .. } => {}
        }
    }
    SummaryRequest::new(
        replaces_up_to,
        selector,
        ModelRequest::new(blocks, Vec::new()),
    )
}

fn recent_indices(entries: &[ContextPlanEntry], keep_recent_tokens: u32) -> HashSet<usize> {
    let mut keep = HashSet::new();
    let mut tokens = 0u64;
    let limit = u64::from(keep_recent_tokens);
    for (idx, entry) in entries.iter().enumerate().rev() {
        if matches!(
            entry.disposition,
            ContextDisposition::Included { .. } | ContextDisposition::Truncated { .. }
        ) {
            let est = u64::from(entry.est_tokens);
            if !keep.is_empty() && limit > 0 && tokens.saturating_add(est) > limit {
                break;
            }
            keep.insert(idx);
            tokens = tokens.saturating_add(est);
            if tokens >= limit {
                break;
            }
        }
    }
    keep
}

fn truncate_block(mut block: ContextBlock, max_tokens: u32) -> ContextBlock {
    let mut remaining = (max_tokens as usize).saturating_mul(4).max(1);
    for part in &mut block.content {
        match part {
            ContentPart::Text(text) => {
                *text = truncate_string(text, remaining);
                remaining = remaining.saturating_sub(text.chars().count());
            }
            ContentPart::ToolUse { args, .. } => {
                let serialized = args.to_string();
                if serialized.chars().count() > remaining {
                    *args = serde_json::json!({ "truncated": true });
                }
                remaining = remaining.saturating_sub(args.to_string().chars().count());
            }
            ContentPart::ToolResult { result, .. } => {
                let serialized = result.to_string();
                if serialized.chars().count() > remaining {
                    *result = Value::String(truncate_string(&serialized, remaining));
                }
                remaining = remaining.saturating_sub(result.to_string().chars().count());
            }
        }
        if remaining == 0 {
            break;
        }
    }
    block
}

fn truncate_string(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    const MARKER: &str = "[...truncated...]";
    if max_chars <= MARKER.len() {
        return MARKER.chars().take(max_chars).collect();
    }
    let keep = max_chars - MARKER.len() - 1;
    format!("{}\n{MARKER}", text.chars().take(keep).collect::<String>())
}

fn totals_for(entries: &[ContextPlanEntry]) -> ContextBudgetTotals {
    let mut totals = ContextBudgetTotals::new();
    for entry in entries {
        totals.add(&entry.disposition, entry.est_tokens);
    }
    totals
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

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn truncation_reduces_non_text_tool_payloads() {
        let block = ContextBlock::new(
            Role::Tool,
            vec![ContentPart::ToolResult {
                id: "call-1".to_string(),
                result: Value::String("x".repeat(50_000)),
            }],
        );
        let truncated = truncate_block(block, 100);
        let ContentPart::ToolResult { result, .. } = &truncated.content[0] else {
            panic!("tool result shape must be preserved");
        };
        assert!(result.as_str().unwrap().chars().count() <= 400);
    }
}
