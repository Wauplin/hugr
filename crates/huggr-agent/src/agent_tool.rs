//! Agents as tools.
//!
//! A Huggr agent *is* a tool: because every agent speaks the same [`Ask`] →
//! [`Answer`] contract, granting one agent to another is one ordinary
//! capability. [`AgentTool`] wraps a **resolver** — an async closure the host
//! (the toolkit's `build_agent`) supplies that runs the child as a subprocess
//! artifact speaking the CLI JSON contract. To the calling model it looks like
//! any tool: its args are an `Ask` (question +
//! optional `trace_id` for follow-ups/forks + blob handles), its result is the
//! child's full `Answer` — so the caller can resume the child's thread across
//! its own turns.
//!
//! **Cost folds upward.** Each invocation pushes the child's [`AnswerMeta`] into
//! a shared sink the parent's [`Agent::ask`](crate::Agent::ask) drains after the
//! turn, so the child's tokens/cost/calls roll into the parent's reported
//! `AnswerMeta` — the orchestrator's cost line stays complete.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

use crate::contract::{Answer, AnswerMeta, Ask};

/// An async closure that runs a child agent for one [`Ask`] and returns its
/// [`Answer`] (or an error string, surfaced to the model as a tool error). The
/// host supplies this — a built artifact spawned as a subprocess speaking the
/// CLI JSON contract.
pub type AgentToolResolver =
    Arc<dyn Fn(Ask) -> Pin<Box<dyn Future<Output = Result<Answer, String>> + Send>> + Send + Sync>;

/// Declares one agent-as-tool grant (`[tools.agent.<name>]`): the capability
/// name (`agent_<name>`), a human description (from the child's `AgentCard`),
/// and the resolver that runs it.
#[derive(Clone)]
pub struct AgentToolSpec {
    pub name: String,
    pub description: String,
    pub resolver: AgentToolResolver,
    /// Directory roots the *calling* agent may already read. A model-supplied
    /// `Path` blob ref outside these is rejected before the child runs, so
    /// delegation never widens filesystem access (empty = no `Path` refs).
    pub readable_roots: Vec<PathBuf>,
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
            readable_roots: Vec::new(),
        }
    }

    /// Allow model-supplied `Path` blob refs under these roots (the caller's
    /// own read jails).
    pub fn with_readable_roots(mut self, roots: Vec<PathBuf>) -> Self {
        self.readable_roots = roots;
        self
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
        format!("Delegate a sub-question to the `{name}` huglet. {description}"),
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question for the huglet." },
                "trace_id": { "type": "string", "description": "Resume/fork the huglet's prior trace (from an earlier Answer)." },
                "blobs": {
                    "type": "array",
                    "description": "Blob handles to forward to the huglet. A `path` ref is only accepted for files this agent can already read; use `bytes` or `sha256` otherwise.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "ref": {
                                "type": "object",
                                "description": "One of {\"kind\":\"bytes\",\"base64\":\"…\"}, {\"kind\":\"sha256\",\"sha256\":\"sha256:<64 hex>\"}, or {\"kind\":\"path\",\"path\":\"…\"} (path must be inside a readable root)."
                            },
                            "media_type": { "type": "string", "description": "IANA media type of the payload." },
                            "name": { "type": "string", "description": "Suggested file name inside the huglet's scratchpad." }
                        },
                        "required": ["ref", "media_type"],
                        "additionalProperties": false
                    }
                }
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
    readable_roots: Vec<PathBuf>,
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
            readable_roots: spec.readable_roots.clone(),
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
        crate::blobs::validate_model_blobs(&ask.blobs, &self.readable_roots)
            .map_err(|e| json!({ "error": e }))?;
        match (self.resolver)(ask).await {
            Ok(answer) => {
                self.spend.lock().unwrap().push(answer.metadata.clone());
                Ok(serde_json::to_value(&answer).unwrap_or(Value::Null))
            }
            Err(err) => Err(json!({ "error": err })),
        }
    }
}

/// A resolver that always reports `agent_depth_exceeded` — the cycle/recursion
/// cut when `max_agent_depth` is reached. No child is ever run.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::{BlobHandle, BlobRef};

    fn echo_resolver(calls: Arc<Mutex<u32>>) -> AgentToolResolver {
        Arc::new(move |_ask: Ask| {
            let calls = calls.clone();
            Box::pin(async move {
                *calls.lock().unwrap() += 1;
                Ok(Answer::default())
            })
        })
    }

    fn ask_args_with_blob(blob_ref: BlobRef) -> Value {
        serde_json::to_value(Ask {
            question: "q".into(),
            blobs: vec![BlobHandle {
                blob_ref,
                media_type: "text/plain".into(),
                name: None,
            }],
            ..Ask::default()
        })
        .unwrap()
    }

    async fn invoke_with(roots: Vec<PathBuf>, blob_ref: BlobRef) -> (Result<Value, Value>, u32) {
        let calls = Arc::new(Mutex::new(0));
        let spec = AgentToolSpec::new("agent_x", "child", echo_resolver(calls.clone()))
            .with_readable_roots(roots);
        let tool = AgentTool::new(&spec, Arc::new(Mutex::new(Vec::new())));
        let sink = ChunkSink::noop();
        let result = tool.invoke(ask_args_with_blob(blob_ref), &sink).await;
        let count = *calls.lock().unwrap();
        (result, count)
    }

    #[tokio::test]
    async fn path_blob_outside_readable_roots_is_rejected_before_the_child_runs() {
        let (result, calls) = invoke_with(
            Vec::new(),
            BlobRef::Path {
                path: "/etc/passwd".into(),
            },
        )
        .await;
        let err = result.unwrap_err();
        assert!(err["error"].as_str().unwrap().contains("outside"), "{err}");
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn path_blob_inside_a_readable_root_is_forwarded() {
        let dir = std::env::temp_dir().join(format!("huggr-agent-tool-in-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("input.txt");
        std::fs::write(&file, b"data").unwrap();
        let (result, calls) = invoke_with(
            vec![dir.clone()],
            BlobRef::Path {
                path: file.display().to_string(),
            },
        )
        .await;
        assert!(result.is_ok());
        assert_eq!(calls, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn traversal_out_of_a_readable_root_is_rejected() {
        let dir = std::env::temp_dir().join(format!("huggr-agent-tool-esc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let jail = dir.join("jail");
        std::fs::create_dir_all(&jail).unwrap();
        std::fs::write(dir.join("secret.txt"), b"s").unwrap();
        let escape = format!("{}/../secret.txt", jail.display());
        let (result, calls) = invoke_with(vec![jail], BlobRef::Path { path: escape }).await;
        assert!(result.is_err());
        assert_eq!(calls, 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn malformed_sha256_is_rejected() {
        let (result, calls) = invoke_with(
            Vec::new(),
            BlobRef::Sha256 {
                sha256: "sha256:../../outside".into(),
            },
        )
        .await;
        assert!(result.is_err());
        assert_eq!(calls, 0);
    }

    #[tokio::test]
    async fn valid_sha256_and_bytes_blobs_pass() {
        let hash = format!("sha256:{}", "a".repeat(64));
        let (result, _) = invoke_with(Vec::new(), BlobRef::Sha256 { sha256: hash }).await;
        assert!(result.is_ok());
        let (result, _) = invoke_with(
            Vec::new(),
            BlobRef::Bytes {
                base64: "aGk=".into(),
            },
        )
        .await;
        assert!(result.is_ok());
    }
}
