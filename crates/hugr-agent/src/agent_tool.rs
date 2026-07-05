//! Agents as tools (ARCHITECTURE §20.5, ROADMAP T3.8).
//!
//! A Hugr agent *is* a tool: because every agent speaks the same [`Ask`] →
//! [`Answer`] contract, granting one agent to another is one ordinary
//! capability. [`AgentTool`] wraps a **resolver** — an async closure the host
//! (the toolkit's `build_agent`) supplies that runs the child, whether
//! in-process (interpreter path) or as a subprocess artifact (§21.1). To the
//! calling model it looks like any tool: its args are an `Ask` (question +
//! optional `trace_id` for follow-ups/forks + blob handles), its result is the
//! child's full `Answer` — so the caller can resume the child's thread across
//! its own turns.
//!
//! **Cost folds upward.** Each invocation pushes the child's [`AnswerMeta`] into
//! a shared sink the parent's [`Agent::ask`](crate::Agent::ask) drains after the
//! turn, so the child's tokens/cost/calls roll into the parent's reported
//! `AnswerMeta` (§18.4) — the orchestrator's cost line stays complete.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde_json::json;

use crate::contract::{Answer, AnswerMeta, Ask};

/// An async closure that runs a child agent for one [`Ask`] and returns its
/// [`Answer`] (or an error string, surfaced to the model as a tool error). The
/// host supplies this — interpreter (in-process child `Agent`) or subprocess
/// (a built artifact speaking the CLI JSON contract).
pub type AgentToolResolver = Arc<
    dyn Fn(Ask) -> Pin<Box<dyn Future<Output = Result<Answer, String>> + Send>> + Send + Sync,
>;

/// Declares one agent-as-tool grant (`[tools.agent.<name>]`): the capability
/// name (`agent_<name>`), a human description (from the child's `AgentCard`),
/// and the resolver that runs it.
#[non_exhaustive]
pub struct AgentToolSpec {
    pub name: String,
    pub description: String,
    pub resolver: AgentToolResolver,
}

impl AgentToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        resolver: AgentToolResolver,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            resolver,
        }
    }

    /// The tool schema the calling model sees — shared by the runtime capability
    /// and by `Agent::describe` so the card matches ask-time capabilities.
    pub fn schema(&self) -> ToolSchema {
        agent_tool_schema(&self.name, &self.description)
    }
}

/// Build the `agent_<name>` tool schema (shared by [`AgentToolSpec::schema`] and
/// the runtime [`AgentTool`]).
fn agent_tool_schema(name: &str, description: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        format!("Delegate a sub-question to the `{name}` subagent. {description}"),
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question for the subagent." },
                "trace_id": { "type": "string", "description": "Resume/fork the subagent's prior trace (from an earlier Answer)." }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
    )
}

/// The capability form of an agent-as-tool grant. Built fresh per ask so its
/// `spend` sink is scoped to that ask (no cross-ask contamination).
pub(crate) struct AgentTool {
    name: String,
    description: String,
    resolver: AgentToolResolver,
    /// Child answer metas from this ask's invocations, folded into the parent's
    /// `AnswerMeta` after the turn.
    spend: Arc<Mutex<Vec<AnswerMeta>>>,
}

impl AgentTool {
    pub(crate) fn new(spec: &AgentToolSpec, spend: Arc<Mutex<Vec<AnswerMeta>>>) -> Self {
        Self {
            name: spec.name.clone(),
            description: spec.description.clone(),
            resolver: spec.resolver.clone(),
            spend,
        }
    }
}

#[async_trait]
impl Capability for AgentTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn schema(&self) -> ToolSchema {
        agent_tool_schema(&self.name, &self.description)
    }

    fn requires_permission(&self) -> bool {
        // Delegation is a pre-vetted manifest grant; the child runs under its
        // own jail. No per-call permission gate (like the other library tools).
        false
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let ask: Ask = serde_json::from_value(args)
            .map_err(|e| json!({ "error": format!("invalid ask for agent tool: {e}") }))?;
        match (self.resolver)(ask).await {
            Ok(answer) => {
                // Fold the child's spend into the parent (§18.4).
                self.spend.lock().unwrap().push(answer.metadata.clone());
                Ok(serde_json::to_value(&answer).unwrap_or(Value::Null))
            }
            Err(err) => Err(json!({ "error": err })),
        }
    }
}

/// A resolver that always reports `agent_depth_exceeded` — the cycle/recursion
/// cut when `max_agent_depth` is reached (§20.5). No child is ever run.
pub fn depth_exceeded_resolver(child: String) -> AgentToolResolver {
    Arc::new(move |_ask: Ask| {
        let child = child.clone();
        Box::pin(async move {
            Err(format!(
                "agent_depth_exceeded: refusing to call `{child}` (max_agent_depth reached)"
            ))
        })
    })
}
