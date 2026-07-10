use std::collections::BTreeMap;
use std::sync::Arc;

use hugr_core::{OpId, OpOutcome, Record};
use hugr_replay::Trace;
use serde::{Deserialize, Serialize};

use crate::agent::Pricing;
use crate::contract::{Answer, TraceId};
use crate::feedback::{FeedbackBackend, FeedbackError};
use crate::store::{StoreError, TraceBackend, TraceHead};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<TraceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceId>,
}

impl StatsOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn since(mut self, trace_id: TraceId) -> Self {
        self.since = Some(trace_id);
        self
    }

    pub fn trace(mut self, trace_id: TraceId) -> Self {
        self.trace = Some(trace_id);
        self
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStats {
    pub ask_count: u64,
    pub feedback_count: u64,
    pub totals: StatsTotals,
    pub duration: DurationStats,
    pub traces: Vec<TraceStats>,
    pub models: Vec<ModelStats>,
    pub tools: Vec<ToolStats>,
    pub children: Vec<ChildAgentStats>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceListing {
    pub trace_id: TraceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<TraceId>,
    pub agent_name: String,
    pub agent_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    pub question: String,
    pub status: String,
    pub feedback_count: u64,
}

impl TraceListing {
    pub fn new(head: TraceHead, feedback_count: u64) -> Self {
        Self {
            trace_id: head.trace_id,
            depends_on: head.depends_on,
            agent_name: head.agent_name,
            agent_version: head.agent_version,
            created_at: head.created_at,
            question: head.question,
            status: head.status,
            feedback_count,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStats {
    pub trace_id: TraceId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depends_on: Option<TraceId>,
    pub question: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
    pub feedback_count: u64,
    pub totals: StatsTotals,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatsTotals {
    pub duration_ms: u64,
    pub cost_micro_usd: u64,
    pub cost_own_micro_usd: u64,
    pub cost_delegated_micro_usd: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
}

impl StatsTotals {
    fn add(&mut self, other: &StatsTotals) {
        self.duration_ms += other.duration_ms;
        self.cost_micro_usd += other.cost_micro_usd;
        self.cost_own_micro_usd += other.cost_own_micro_usd;
        self.cost_delegated_micro_usd += other.cost_delegated_micro_usd;
        self.tokens_in += other.tokens_in;
        self.tokens_out += other.tokens_out;
        self.model_calls += other.model_calls;
        self.tool_calls += other.tool_calls;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurationStats {
    pub mean_ms: u64,
    pub median_ms: u64,
    pub p95_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStats {
    pub selector: String,
    pub calls: u32,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub cost_micro_usd: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolStats {
    pub name: String,
    pub calls: u32,
    pub error_count: u32,
    pub total_latency_ms: u64,
    pub mean_latency_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildAgentStats {
    pub name: String,
    pub calls: u32,
    pub cost_delegated_micro_usd: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AnalyticsError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Feedback(#[from] FeedbackError),
}

pub async fn collect_stats(
    traces: Arc<dyn TraceBackend>,
    feedback: Arc<dyn FeedbackBackend>,
    pricing: &Pricing,
    options: StatsOptions,
) -> Result<AgentStats, AnalyticsError> {
    let mut heads = traces.list().await?;
    heads = filter_heads(heads, &options)?;

    let mut out = AgentStats::default();
    let mut durations = Vec::new();
    let mut models: BTreeMap<String, ModelStats> = BTreeMap::new();
    let mut tools: BTreeMap<String, ToolStats> = BTreeMap::new();
    let mut children: BTreeMap<String, ChildAgentStats> = BTreeMap::new();

    for head in heads {
        let trace = traces.get(&head.trace_id).await?;
        let baseline = baseline_len(traces.as_ref(), &head).await?;
        let feedback_count = feedback.list(&head.trace_id).await?.len() as u64;
        let totals = fold_trace(
            &trace,
            baseline.min(trace.log.len()),
            pricing,
            &mut models,
            &mut tools,
            &mut children,
        );
        durations.push(totals.duration_ms);
        out.feedback_count += feedback_count;
        out.totals.add(&totals);
        out.traces.push(TraceStats {
            trace_id: head.trace_id,
            depends_on: head.depends_on,
            question: head.question,
            status: head.status,
            created_at: head.created_at,
            feedback_count,
            totals,
        });
    }

    out.ask_count = out.traces.len() as u64;
    out.duration = duration_stats(durations);
    out.models = models.into_values().collect();
    out.tools = tools.into_values().collect();
    out.children = children.into_values().collect();
    Ok(out)
}

pub async fn list_traces_with_feedback(
    traces: Arc<dyn TraceBackend>,
    feedback: Arc<dyn FeedbackBackend>,
) -> Result<Vec<TraceListing>, AnalyticsError> {
    let mut listings = Vec::new();
    for head in traces.list().await? {
        let feedback_count = feedback.list(&head.trace_id).await?.len() as u64;
        listings.push(TraceListing::new(head, feedback_count));
    }
    Ok(listings)
}

fn filter_heads(
    mut heads: Vec<TraceHead>,
    options: &StatsOptions,
) -> Result<Vec<TraceHead>, StoreError> {
    if let Some(trace_id) = &options.trace {
        let Some(head) = heads.into_iter().find(|head| &head.trace_id == trace_id) else {
            return Err(StoreError::NotFound {
                id: trace_id.clone(),
            });
        };
        return Ok(vec![head]);
    }

    if let Some(since) = &options.since {
        let Some(anchor) = heads.iter().find(|head| &head.trace_id == since) else {
            return Err(StoreError::NotFound { id: since.clone() });
        };
        match anchor.created_at {
            Some(anchor_created_at) => {
                heads.retain(|head| head.created_at.unwrap_or(0) >= anchor_created_at);
            }
            None => {
                heads.retain(|head| head.trace_id >= *since);
            }
        }
    }
    Ok(heads)
}

async fn baseline_len(
    traces: &dyn TraceBackend,
    head: &TraceHead,
) -> Result<usize, AnalyticsError> {
    let Some(parent) = &head.depends_on else {
        return Ok(0);
    };
    match traces.get(parent).await {
        Ok(parent_trace) => Ok(parent_trace.log.len()),
        Err(StoreError::NotFound { .. }) => Ok(0),
        Err(err) => Err(err.into()),
    }
}

fn fold_trace(
    trace: &Trace,
    baseline: usize,
    pricing: &Pricing,
    models: &mut BTreeMap<String, ModelStats>,
    tools: &mut BTreeMap<String, ToolStats>,
    children: &mut BTreeMap<String, ChildAgentStats>,
) -> StatsTotals {
    let mut totals = StatsTotals::default();
    let mut tool_names: BTreeMap<OpId, String> = BTreeMap::new();
    let mut child_cost = 0;
    let mut first_started = None;
    let mut last_ended = None;

    for entry in &trace.log[baseline..] {
        if let Record::ToolResult {
            op, name, result, ..
        } = &entry.record
        {
            tool_names.insert(*op, name.clone());
            if let Some(child_name) = child_agent_name(name)
                && let Ok(answer) = serde_json::from_value::<Answer>(result.clone())
            {
                child_cost += answer.metadata.cost_micro_usd;
                let child = children.entry(child_name.to_string()).or_default();
                child.name = child_name.to_string();
                child.calls += 1;
                child.cost_delegated_micro_usd += answer.metadata.cost_micro_usd;
            }
        }
    }

    for entry in &trace.log[baseline..] {
        let Record::OpEnded { op, outcome, meta } = &entry.record else {
            continue;
        };
        first_started = Some(
            first_started
                .unwrap_or(meta.started_at)
                .min(meta.started_at),
        );
        last_ended = Some(last_ended.unwrap_or(meta.ended_at).max(meta.ended_at));

        if let (Some(selector), Some(usage)) = (&meta.model, &meta.usage) {
            let selector = selector.0.clone();
            let cost = pricing.cost_micro_usd(&selector, usage.input_tokens, usage.output_tokens);
            totals.model_calls += 1;
            totals.tokens_in += usage.input_tokens;
            totals.tokens_out += usage.output_tokens;
            totals.cost_own_micro_usd += cost;

            let stat = models.entry(selector.clone()).or_default();
            stat.selector = selector;
            stat.calls += 1;
            stat.tokens_in += usage.input_tokens;
            stat.tokens_out += usage.output_tokens;
            stat.cost_micro_usd += cost;
        } else if meta.model.is_none() {
            totals.tool_calls += 1;
            let name = tool_names
                .get(op)
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string());
            let latency = meta.ended_at.0.saturating_sub(meta.started_at.0);
            let stat = tools.entry(name.clone()).or_default();
            stat.name = name;
            stat.calls += 1;
            stat.total_latency_ms += latency;
            stat.mean_latency_ms = stat.total_latency_ms / u64::from(stat.calls);
            if !matches!(outcome, OpOutcome::Ok) {
                stat.error_count += 1;
            }
        }
    }

    totals.duration_ms = match (first_started, last_ended) {
        (Some(started), Some(ended)) => ended.0.saturating_sub(started.0),
        _ => 0,
    };
    totals.cost_delegated_micro_usd = child_cost;
    totals.cost_micro_usd = totals.cost_own_micro_usd + totals.cost_delegated_micro_usd;
    totals
}

fn child_agent_name(tool_name: &str) -> Option<&str> {
    let child = tool_name.strip_prefix("agent_")?;
    if child.ends_with("_feedback") {
        return None;
    }
    Some(child)
}

fn duration_stats(mut durations: Vec<u64>) -> DurationStats {
    if durations.is_empty() {
        return DurationStats::default();
    }
    durations.sort_unstable();
    let total: u64 = durations.iter().sum();
    DurationStats {
        mean_ms: total / durations.len() as u64,
        median_ms: percentile(&durations, 50),
        p95_ms: percentile(&durations, 95),
    }
}

fn percentile(sorted: &[u64], p: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) * p).div_ceil(100);
    sorted[idx]
}
