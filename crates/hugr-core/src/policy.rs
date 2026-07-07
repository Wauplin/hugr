//! The pluggable turn strategy.
//!
//! `TurnPolicy` is the **only place agent strategy lives** (ARCHITECTURE §2.5).
//! The reducer asks it which model to call, how to project context from the
//! log, and whether a capability needs permission — but never hardcodes those
//! decisions. Swap the policy to change behaviour without touching the reducer.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::model::{
    ContentPart, ContextBlock, ContextBudgetTotals, ContextDisposition, ContextPlan,
    ContextPlanEntry, ContextSource, ModelOutput, ModelRequest, ModelSelector, Role,
    SamplingParams, TokenBudget, ToolSchema, ToolVersioning,
};
use crate::primitives::{Seq, Value};
use crate::record::{LogEntry, Record, SeqRange, SummaryCoverage};
use crate::state::BrainState;

/// Decode a policy captured as an opaque [`Value`] — e.g. a trace's stored
/// policy config. Returns `None` when the value does not decode as a
/// [`StaticPolicy`] (e.g. a custom host policy); the caller picks its own
/// fallback. Faithful replay needs the *same* policy a session was recorded
/// under — the brain branches on its pure decisions (ARCHITECTURE §6.3).
pub fn decode_policy(value: &Value) -> Option<Box<dyn TurnPolicy>> {
    serde_json::from_value::<StaticPolicy>(value.clone())
        .ok()
        .map(|policy| Box::new(policy) as Box<dyn TurnPolicy>)
}

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

/// The exact source span selected for one compaction pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompactionTarget {
    pub summary_of: SeqRange,
    pub est_tokens_in: u32,
}

impl CompactionTarget {
    pub fn new(summary_of: SeqRange, est_tokens_in: u32) -> Self {
        Self {
            summary_of,
            est_tokens_in,
        }
    }
}

/// Strategy for driving the turn loop. Implementations must be **pure**:
/// [`project_context`](TurnPolicy::project_context) only *reads* the log (no IO,
/// no model calls — compaction is a separate model op, ARCHITECTURE §3.4).
///
/// `Send + Sync` so the host may move the whole brain onto a worker task — the
/// brain is still reduced single-threaded (CLAUDE.md); this only lets a host
/// (e.g. a sub-agent runner, ARCHITECTURE §13.2) own a brain on another thread.
pub trait TurnPolicy: Send + Sync {
    /// Pick which logical model to call for the next step. Pure: derive the
    /// choice only from `state` — this is the only place a selector is decided
    /// for model turns (ARCHITECTURE §2.5).
    fn choose_model(&self, state: &BrainState) -> ModelSelector;

    /// Pick the token budget the next context projection plans against.
    fn context_budget(&self, _state: &BrainState) -> TokenBudget {
        TokenBudget::default()
    }

    /// Plan the model context from the log. Pure and synchronous: include /
    /// summarize / evict-to-reference / drop. Must never call a model.
    fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan;

    /// Token high-water mark that triggers one compaction pass before the real
    /// turn model call. `None` disables automatic compaction.
    fn compaction_high_water(&self, _state: &BrainState, _budget: TokenBudget) -> Option<u64> {
        None
    }

    /// Pick the exact log span to summarize once the high-water mark is crossed.
    fn select_compaction_span(
        &self,
        log: &[LogEntry],
        plan: &ContextPlan,
    ) -> Option<CompactionTarget> {
        default_compaction_target(log, plan)
    }

    /// Build the model request for one compaction pass over `summary_of`.
    ///
    /// The default keeps the built-in summarization strategy **in the core** so
    /// every host gets it for free (ARCHITECTURE §3.4): an English summarization
    /// prompt plus a per-record rendering of the span (see
    /// [`render_summary_record`](Self::render_summary_record)). Override this to
    /// customize the prompt, language, or format without touching the reducer
    /// (agent strategy lives in the policy, ARCHITECTURE §2.5). This is pure —
    /// it only *reads* the log; compaction is a separate model op (§3.4).
    fn compaction_request(&self, log: &[LogEntry], summary_of: SeqRange) -> ModelRequest {
        let mut rendered = String::new();
        for entry in log.iter().filter(|entry| summary_of.contains(entry.seq)) {
            if let Some(line) = self.render_summary_record(entry.seq, &entry.record) {
                if !rendered.is_empty() {
                    rendered.push('\n');
                }
                rendered.push_str(&line);
            }
        }
        let mut request = ModelRequest::new(
            vec![
                ContextBlock::new(
                    Role::System,
                    vec![ContentPart::Text(COMPACTION_SYSTEM_PROMPT.to_string())],
                ),
                ContextBlock::new(Role::User, vec![ContentPart::Text(rendered)]),
            ],
            Vec::new(),
            SamplingParams::default(),
        );
        request.extra = serde_json::json!({
            "kind": "compaction",
            "summary_of": {
                "start": summary_of.start.0,
                "end": summary_of.end.0,
            },
        });
        request
    }

    /// Render one durable log record into a single line of summarization input,
    /// or `None` to omit it (e.g. pure `OpEnded` bookkeeping). A *provided*
    /// method so the default rendering lives here and the reducer never needs an
    /// edit when a new `Record` variant is added (ARCHITECTURE §2.4). Override to
    /// change the wording/format a summary is built from.
    fn render_summary_record(&self, seq: Seq, record: &Record) -> Option<String> {
        default_render_summary_record(seq, record)
    }

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

    /// Declarative optimistic-concurrency metadata for a capability, if any.
    /// The reducer uses this to stamp `expected_version` without hardcoding
    /// capability-specific argument shapes (ARCHITECTURE §7.3).
    fn capability_versioning(&self, _capability: &str) -> Option<ToolVersioning> {
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
    #[serde(default)]
    context_budget: TokenBudget,
    /// Percentage of the budget that triggers automatic compaction. `0`
    /// disables it. Defaults to 90%.
    #[serde(default = "default_compaction_high_water_percent")]
    compaction_high_water_percent: u8,
}

impl Default for StaticPolicy {
    fn default() -> Self {
        Self {
            model: ModelSelector::named("medium"),
            tools: Vec::new(),
            permissioned: Vec::new(),
            background: Vec::new(),
            agents: Vec::new(),
            params: SamplingParams::default(),
            system: None,
            context_budget: TokenBudget::default(),
            compaction_high_water_percent: default_compaction_high_water_percent(),
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

    /// Set the approximate input token budget used by context planning.
    pub fn with_context_budget(mut self, budget: TokenBudget) -> Self {
        self.context_budget = budget;
        self
    }

    /// Set the percentage of the context budget that triggers automatic
    /// compaction. Use `0` to disable automatic compaction.
    pub fn with_compaction_high_water_percent(mut self, percent: u8) -> Self {
        self.compaction_high_water_percent = percent;
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
        // One context block per logged message / result, in log order. Durable
        // summaries evict covered source records to references without deleting
        // the originals (ARCHITECTURE §3.4).
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
            note: &str,
        ) {
            totals.add(&disposition, est_tokens);
            entries.push(ContextPlanEntry::new(source, est_tokens, disposition, note));
        }
        let summaries = complete_summaries(log);
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
                "static system prompt",
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
                    "tool result projected adjacent to originating tool call",
                ));
                continue;
            }
            if let Some(summary_seq) = covering_summary(&summaries, entry.seq) {
                let disposition = ContextDisposition::referenced(ContextBlock::new(
                    Role::User,
                    vec![ContentPart::Ref {
                        reference: format!("log:{}", entry.seq.0),
                        summary: format!("covered by summary log:{}", summary_seq.0),
                        est_tokens: 1,
                    }],
                ));
                push(
                    &mut totals,
                    &mut entries,
                    ContextSource::log_entry(entry.seq),
                    1,
                    disposition,
                    "source entry is covered by a durable summary",
                );
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
                        "static pass-through projection",
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
                            "static pass-through projection",
                        );
                    }
                    // OpenAI-compatible chat formats require tool result
                    // messages to immediately follow the assistant message
                    // containing the corresponding `tool_calls`. Durable host
                    // hooks and op metadata can be logged between those facts,
                    // so projection groups matching results here without
                    // changing the append-only log (ARCHITECTURE §2.4/§4.5).
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
                                    "tool result grouped with originating tool call",
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
                        "static pass-through projection",
                    );
                }
                Record::Summary {
                    text,
                    summary_of,
                    coverage,
                    tier,
                    est_tokens_in,
                    est_tokens_out,
                    ..
                } => {
                    let coverage_label = match coverage {
                        SummaryCoverage::Complete => "complete".to_string(),
                        SummaryCoverage::Partial { reason } => format!("partial: {reason}"),
                    };
                    let disposition = ContextDisposition::summarized(ContextBlock::new(
                        Role::Assistant,
                        vec![ContentPart::Text(format!(
                            "Summary of log:{}..log:{} ({coverage_label}, tier {:?}, {} -> {} est tokens):\n{}",
                            summary_of.start.0,
                            summary_of.end.0,
                            tier,
                            est_tokens_in,
                            est_tokens_out,
                            text
                        ))],
                    ));
                    push(
                        &mut totals,
                        &mut entries,
                        ContextSource::log_entry(entry.seq),
                        *est_tokens_out,
                        disposition,
                        "durable summary projection",
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
                        "operation metadata is not model context",
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
        .with_extra(json!(null))
    }

    fn needs_permission(&self, capability: &str) -> bool {
        self.permissioned.iter().any(|c| c == capability)
    }

    fn compaction_high_water(&self, _state: &BrainState, budget: TokenBudget) -> Option<u64> {
        let percent = u64::from(self.compaction_high_water_percent.min(100));
        if percent == 0 || budget.max_tokens == 0 {
            return None;
        }
        Some((budget.max_tokens.saturating_mul(percent) / 100).max(1))
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

    fn capability_versioning(&self, capability: &str) -> Option<ToolVersioning> {
        self.tools
            .iter()
            .find(|tool| tool.name == capability)
            .and_then(|tool| tool.versioning.clone())
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

fn default_compaction_high_water_percent() -> u8 {
    90
}

fn default_compaction_target(log: &[LogEntry], plan: &ContextPlan) -> Option<CompactionTarget> {
    let compactable: Vec<_> = plan
        .entries
        .iter()
        .filter_map(|entry| {
            let ContextSource::LogEntry { seq } = entry.source else {
                return None;
            };
            if !matches!(entry.disposition, ContextDisposition::Included { .. }) {
                return None;
            }
            // The log is append-only and `seq`-ordered, so a binary search
            // replaces what would otherwise be a linear scan per plan entry.
            let record = log
                .binary_search_by_key(&seq, |candidate| candidate.seq)
                .ok()
                .map(|index| &log[index])?;
            if !is_compactable_record(&record.record) {
                return None;
            }
            Some((seq, record.record.content_est_tokens().unwrap_or(0)))
        })
        .collect();

    if compactable.len() < 2 {
        return None;
    }

    let keep_tail = 1;
    let candidates = &compactable[..compactable.len().saturating_sub(keep_tail)];
    let (start, _) = *candidates.first()?;
    let mut end = start;
    let mut est_tokens_in = 0_u32;
    let mut records = 0_usize;
    let target = (plan.budget.max_tokens / 2).max(1);

    for (seq, tokens) in candidates {
        end = *seq;
        est_tokens_in = est_tokens_in.saturating_add(*tokens);
        records += 1;
        if records >= 2 && u64::from(est_tokens_in) >= target {
            break;
        }
    }

    // Never let the span boundary fall *inside* a tool_use/tool_result group.
    // If `end` landed on a `ModelOutput` carrying tool calls, its answering
    // `ToolResult`s may sit past `end`; summarizing the assistant tool_use away
    // (projected as a `Ref`) while leaving the paired tool_result `Included`
    // yields an orphaned tool_result and a provider 400 — exactly when
    // auto-compaction was meant to rescue the session. Extend `end` to swallow
    // the whole group so the follow-up projection is always a valid provider
    // message sequence (ARCHITECTURE §2.4/§4.5).
    let (end, est_tokens_in) = extend_past_tool_group(log, end, est_tokens_in);

    if est_tokens_in == 0 {
        return None;
    }

    Some(CompactionTarget::new(
        SeqRange::new(start, end),
        est_tokens_in,
    ))
}

/// If the record at `end` is a `ModelOutput` with tool calls, extend `end` to
/// the last `ToolResult` answering them (adding their token estimates), so a
/// compaction span never splits a tool_use/tool_result group (ARCHITECTURE
/// §2.4/§4.5). Returns the (possibly unchanged) end seq and token total. Any
/// call whose result is not yet in the log is left as-is: projection renders the
/// covered `ModelOutput` as a `Ref`, dropping its tool_use entirely, so no
/// dangling tool_use reaches the provider either.
fn extend_past_tool_group(
    log: &[LogEntry],
    end: crate::primitives::Seq,
    mut est_tokens_in: u32,
) -> (crate::primitives::Seq, u32) {
    let Some(entry) = log
        .binary_search_by_key(&end, |candidate| candidate.seq)
        .ok()
        .map(|index| &log[index])
    else {
        return (end, est_tokens_in);
    };
    let Record::ModelOutput { output, .. } = &entry.record else {
        return (end, est_tokens_in);
    };
    let mut new_end = end;
    for call in &output.tool_calls {
        if let Some(result) = find_tool_result_for_call(log, end, &call.id) {
            if result.seq > new_end {
                new_end = result.seq;
                est_tokens_in =
                    est_tokens_in.saturating_add(result.record.content_est_tokens().unwrap_or(0));
            }
        }
    }
    (new_end, est_tokens_in)
}

/// The built-in English summarization system prompt. Kept here (not in the
/// reducer) so hosts inherit it for free yet can override
/// [`compaction_request`](TurnPolicy::compaction_request) (ARCHITECTURE §3.4).
const COMPACTION_SYSTEM_PROMPT: &str = "Summarize the provided Hugr log span for future context. Preserve user intent, decisions, tool results, and unresolved work. Return concise plain text only.";

/// The built-in per-record rendering used to assemble compaction input. Kept as
/// a free fn so [`TurnPolicy::render_summary_record`]'s default can delegate to
/// it and a host override can still fall back for the records it doesn't care to
/// customize.
fn default_render_summary_record(seq: Seq, record: &Record) -> Option<String> {
    match record {
        Record::UserMessage { text, .. } => Some(format!("log:{} user: {}", seq.0, text)),
        Record::ModelOutput { output, .. } => {
            Some(format!("log:{} assistant: {}", seq.0, summary_text(output)))
        }
        Record::ToolResult { name, result, .. } => {
            Some(format!("log:{} tool {}: {}", seq.0, name, result))
        }
        Record::Summary { text, .. } => Some(format!("log:{} summary: {}", seq.0, text)),
        // Turn-control / bookkeeping records contribute nothing to a summary.
        Record::OpEnded { .. } => None,
    }
}

/// The text used to represent a model output in summarization input: its text,
/// or a JSON encoding of its tool calls when it made no textual reply.
fn summary_text(output: &ModelOutput) -> String {
    if !output.text.is_empty() {
        return output.text.clone();
    }
    if output.tool_calls.is_empty() {
        String::new()
    } else {
        serde_json::to_string(&output.tool_calls).unwrap_or_default()
    }
}

fn is_compactable_record(record: &Record) -> bool {
    matches!(
        record,
        Record::UserMessage { .. } | Record::ModelOutput { .. } | Record::ToolResult { .. }
    )
}

fn complete_summaries(log: &[LogEntry]) -> Vec<(crate::primitives::Seq, crate::record::SeqRange)> {
    log.iter()
        .filter_map(|entry| match &entry.record {
            Record::Summary {
                summary_of,
                coverage: SummaryCoverage::Complete,
                ..
            } => Some((entry.seq, *summary_of)),
            _ => None,
        })
        .collect()
}

fn covering_summary(
    summaries: &[(crate::primitives::Seq, crate::record::SeqRange)],
    seq: crate::primitives::Seq,
) -> Option<crate::primitives::Seq> {
    summaries.iter().rev().find_map(|(summary_seq, range)| {
        if *summary_seq != seq && range.contains(seq) {
            Some(*summary_seq)
        } else {
            None
        }
    })
}
