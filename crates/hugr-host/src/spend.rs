//! Trace-derived spend and routing summaries.
//!
//! This is host-side because provider cost lives in `Usage.extra`, an opaque
//! narrow-waist bag the core stores but does not interpret.

use hugr_core::{LogEntry, ModelSelector, Record, RoutingDecision};

use crate::frontend::usage_cost;

#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct SpendReport {
    pub tiers: Vec<TierSpend>,
    pub recent_routing: Vec<RoutingDecision>,
}

impl SpendReport {
    pub fn new(tiers: Vec<TierSpend>, recent_routing: Vec<RoutingDecision>) -> Self {
        Self {
            tiers,
            recent_routing,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct TierSpend {
    pub selector: ModelSelector,
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost: Option<f64>,
    pub latency_ms: u64,
}

impl TierSpend {
    pub fn new(selector: ModelSelector) -> Self {
        Self {
            selector,
            model_calls: 0,
            input_tokens: 0,
            output_tokens: 0,
            cost: None,
            latency_ms: 0,
        }
    }
}

pub fn spend_report(log: &[LogEntry]) -> SpendReport {
    let mut tiers: Vec<TierSpend> = Vec::new();
    let mut routing = Vec::new();

    for entry in log {
        let Record::OpEnded { meta, .. } = &entry.record else {
            continue;
        };
        let Some(selector) = &meta.model else {
            continue;
        };

        let index = match tiers.iter().position(|tier| &tier.selector == selector) {
            Some(index) => index,
            None => {
                tiers.push(TierSpend::new(selector.clone()));
                tiers.len() - 1
            }
        };
        let tier = &mut tiers[index];
        tier.model_calls += 1;
        tier.latency_ms += meta.ended_at.0.saturating_sub(meta.started_at.0);
        if let Some(usage) = &meta.usage {
            tier.input_tokens += usage.input_tokens;
            tier.output_tokens += usage.output_tokens;
            if let Some(cost) = usage_cost(usage) {
                tier.cost = Some(tier.cost.unwrap_or(0.0) + cost);
            }
        }
        if let Some(decision) = &meta.routing {
            if !decision.reasons.is_empty() {
                routing.push(decision.clone());
            }
        }
    }

    SpendReport::new(tiers, routing.into_iter().rev().take(8).collect())
}
