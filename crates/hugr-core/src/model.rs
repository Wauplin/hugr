//! The canonical model representation.
//!
//! These types are the parts of a model call the **brain branches on**: the
//! request it assembles from the log, the streaming deltas it accumulates, and
//! the consolidated output whose `tool_calls` and `stop` reason drive the turn
//! loop. Provider-specific knobs ride in opaque `extra` fields so adding a
//! provider feature never changes a core type.

use serde::{Deserialize, Serialize};

use crate::primitives::{Seq, Value};

/// A logical model **role**, not a concrete endpoint. The brain names a role;
/// the host's model registry resolves it to a provider/model/key/adapter
/// as an open string set, e.g. `"medium"`, `"summarizer"`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelSelector(pub String);

impl ModelSelector {
    /// Convenience constructor: `ModelSelector::named("big")`.
    pub fn named(name: impl Into<String>) -> Self {
        ModelSelector(name.into())
    }
}

/// What the brain sends to a model, rendered from the log by the
/// [`TurnPolicy`](crate::TurnPolicy). Structured, never a concatenated string,
/// so cache breakpoints and reasoning survive the round trip.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelRequest {
    /// Ordered context blocks (system/user/assistant/tool turns).
    pub blocks: Vec<ContextBlock>,
    /// Tool schemas advertised to the model this turn.
    pub tools: Vec<ToolSchema>,
    /// Sampling parameters.
    pub params: SamplingParams,
    /// Provider-specific knobs the brain never reads (narrow-waist passthrough).
    pub extra: Value,
}

impl ModelRequest {
    /// Construct a request. `extra` defaults to null; set it afterwards for
    /// provider-specific passthrough.
    pub fn new(blocks: Vec<ContextBlock>, tools: Vec<ToolSchema>, params: SamplingParams) -> Self {
        Self {
            blocks,
            tools,
            params,
            extra: Value::Null,
        }
    }
}

/// The token budget a projection must plan against.
///
/// Counts are estimates supplied by the host when records enter the log. The
/// brain only sums them; it never tokenizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct TokenBudget {
    pub max_tokens: u64,
}

impl TokenBudget {
    pub fn new(max_tokens: u64) -> Self {
        Self { max_tokens }
    }
}

impl Default for TokenBudget {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
        }
    }
}

/// A pure, inspectable context projection plan.
///
/// The reducer derives a [`ModelRequest`] from this plan; hosts can inspect the
/// same data to explain why each log block was included or omitted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextPlan {
    pub budget: TokenBudget,
    pub entries: Vec<ContextPlanEntry>,
    pub totals: ContextBudgetTotals,
    pub tools: Vec<ToolSchema>,
    pub params: SamplingParams,
    pub extra: Value,
}

impl ContextPlan {
    pub fn new(
        budget: TokenBudget,
        entries: Vec<ContextPlanEntry>,
        totals: ContextBudgetTotals,
        tools: Vec<ToolSchema>,
        params: SamplingParams,
    ) -> Self {
        Self {
            budget,
            entries,
            totals,
            tools,
            params,
            extra: Value::Null,
        }
    }

    /// Render the actual model request sent to the host.
    pub fn to_model_request(&self) -> ModelRequest {
        ModelRequest {
            blocks: self
                .entries
                .iter()
                .filter_map(|entry| entry.disposition.as_request_block().cloned())
                .collect(),
            tools: self.tools.clone(),
            params: self.params.clone(),
            extra: self.extra.clone(),
        }
    }

    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }
}

/// One source block's disposition in a [`ContextPlan`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextPlanEntry {
    pub source: ContextSource,
    pub est_tokens: u32,
    pub disposition: ContextDisposition,
}

impl ContextPlanEntry {
    pub fn new(source: ContextSource, est_tokens: u32, disposition: ContextDisposition) -> Self {
        Self {
            source,
            est_tokens,
            disposition,
        }
    }
}

/// Where a projected context block came from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContextSource {
    System,
    LogEntry { seq: Seq },
}

impl ContextSource {
    pub fn system() -> Self {
        Self::System
    }

    pub fn log_entry(seq: Seq) -> Self {
        Self::LogEntry { seq }
    }
}

/// How a source block is represented in the request, if at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContextDisposition {
    Included { block: ContextBlock },
    Truncated { block: ContextBlock },
    Dropped { note: Option<String> },
    Omitted,
}

impl ContextDisposition {
    pub fn included(block: ContextBlock) -> Self {
        Self::Included { block }
    }

    pub fn omitted() -> Self {
        Self::Omitted
    }

    pub fn truncated(block: ContextBlock) -> Self {
        Self::Truncated { block }
    }

    pub fn dropped(note: Option<String>) -> Self {
        Self::Dropped { note }
    }

    fn as_request_block(&self) -> Option<&ContextBlock> {
        match self {
            ContextDisposition::Included { block } | ContextDisposition::Truncated { block } => {
                Some(block)
            }
            ContextDisposition::Dropped { .. } | ContextDisposition::Omitted => None,
        }
    }
}

/// Token totals for the projection plan. `used_tokens` is the sum of blocks
/// still represented in the request; omitted tokens are tracked separately so
/// truncation is visible rather than silent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextBudgetTotals {
    pub used_tokens: u64,
    pub omitted_tokens: u64,
    pub truncated_tokens: u64,
    pub dropped_tokens: u64,
}

impl ContextBudgetTotals {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, disposition: &ContextDisposition, est_tokens: u32) {
        let est_tokens = u64::from(est_tokens);
        match disposition {
            ContextDisposition::Included { .. } => {
                self.used_tokens += est_tokens;
            }
            ContextDisposition::Truncated { .. } => {
                self.used_tokens += est_tokens;
                self.truncated_tokens += est_tokens;
            }
            ContextDisposition::Dropped { .. } => {
                self.omitted_tokens += est_tokens;
                self.dropped_tokens += est_tokens;
            }
            ContextDisposition::Omitted => {
                self.omitted_tokens += est_tokens;
            }
        }
    }
}

/// One block of model context.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ContextBlock {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl ContextBlock {
    pub fn new(role: Role, content: Vec<ContentPart>) -> Self {
        Self { role, content }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A piece of content within a [`ContextBlock`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ContentPart {
    Text(String),
    /// A tool call the assistant made (echoed back into context).
    ToolUse {
        id: String,
        name: String,
        args: Value,
    },
    /// The result of a tool call, fed back to the model.
    ToolResult {
        id: String,
        result: Value,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SamplingParams {
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl SamplingParams {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = Some(temperature);
        self
    }

    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }
}

/// A tool schema advertised to the model. The brain treats the JSON-schema
/// `parameters` as opaque; only the host and the model interpret it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSchema {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Value) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
        }
    }
}

/// Streaming model output. Hosts always call models in streaming mode (the only
/// supported mode), so these deltas arrive continuously during a model call.
///
/// **Transport only** — deltas accumulate in the op's live buffer and drive
/// cosmetic [`OutputEvent`](crate::OutputEvent)s, but are never written to the
/// log. The brain's logic keys off the consolidated [`ModelOutput`] in
/// [`Event::ModelDone`](crate::Event::ModelDone).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum ModelDelta {
    Text(String),
    Reasoning(String),
    ToolCallStart { id: String, name: String },
}

/// The consolidated, authoritative result of a model call — exactly what the
/// brain reasons about. The presence of `tool_calls` decides whether the turn
/// continues into tool execution or ends.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ModelOutput {
    pub text: String,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Provider-reported stop reason, e.g. `"end_turn"`, `"tool_use"`,
    /// `"max_tokens"`. Recorded for the trace; the brain never branches on it
    /// (the presence of `tool_calls` drives the turn loop).
    pub stop: String,
}

impl ModelOutput {
    /// Full constructor, for adapters assembling a streamed result.
    pub fn new(
        text: String,
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
        stop: impl Into<String>,
    ) -> Self {
        Self {
            text,
            reasoning,
            tool_calls,
            stop: stop.into(),
        }
    }

    /// A final answer with no tool calls — ends the turn.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            stop: "end_turn".to_string(),
            ..Self::default()
        }
    }

    /// A turn that requests the given tool calls.
    pub fn tool_calls(calls: Vec<ToolCall>) -> Self {
        Self {
            tool_calls: calls,
            stop: "tool_use".to_string(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Opaque to the brain; forwarded verbatim to the capability.
    pub args: Value,
}

impl ToolCall {
    pub fn new(id: impl Into<String>, name: impl Into<String>, args: Value) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            args,
        }
    }
}

/// Authoritative token accounting returned by the provider after a call.
///
/// `input_tokens`/`output_tokens` are the only fields the brain (and the host's
/// budgeting) ever needs as numbers. Anything richer the provider reports —
/// notably **cost**, which the brain never branches on — rides in the opaque
/// `extra` field. The brain stores and forwards `extra` verbatim; only the host
/// reads it. Adapters that learn real cost from the router response stash it
/// there rather than baking a cost type into the core.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Provider-reported extras the brain never interprets (e.g. cost). Defaults
    /// to `Value::Null` when the adapter has nothing to add.
    pub extra: Value,
}

impl Usage {
    /// Token-only usage; `extra` defaults to null.
    pub fn new(input_tokens: u64, output_tokens: u64) -> Self {
        Self {
            input_tokens,
            output_tokens,
            extra: Value::Null,
        }
    }

    /// Attach opaque provider extras (e.g. a `{ "cost": … }` object). The brain
    /// forwards this untouched; only the host reads it.
    pub fn with_extra(mut self, extra: Value) -> Self {
        self.extra = extra;
        self
    }
}
