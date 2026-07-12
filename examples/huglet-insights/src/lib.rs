//! Typed response contract for the `huglet-insights` example agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESPONSE_RUST_TYPE: &str = "huglet_insights::InsightsResponse";

/// Structured improvement report mined from another agent's traces + feedback.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InsightsResponse {
    /// Recurring behavioral patterns observed across traces.
    pub patterns: Vec<Pattern>,
    /// Concrete changes to the analyzed agent's system prompt.
    pub prompt_suggestions: Vec<String>,
    /// Tools that should be added, merged, or reshaped.
    pub tool_suggestions: Vec<String>,
    /// The main themes present in caller feedback.
    pub feedback_themes: Vec<String>,
}

/// One observed pattern, grounded in the traces that exhibit it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pattern {
    /// What was observed, and why it matters.
    pub description: String,
    /// Trace ids exhibiting this pattern.
    pub evidence_trace_ids: Vec<String>,
}
