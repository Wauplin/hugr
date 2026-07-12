//! Introspection API: describe/traces expose the same stable data every surface
//! will print.

use std::sync::Arc;

use async_trait::async_trait;
use huggr_agent::{Agent, AgentLimits, Ask, Pricing, TraceStore};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, ToolSchema, Usage, Value};
use huggr_host::{Capability, ChunkSink, ModelAdapter, ModelSink};
use serde_json::json;

struct MockModel;

#[async_trait]
impl ModelAdapter for MockModel {
    async fn call(
        &self,
        _request: ModelRequest,
        _sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        Ok((ModelOutput::text("answer"), Usage::new(5, 2)))
    }
}

struct ReadOnlyTool;

#[async_trait]
impl Capability for ReadOnlyTool {
    fn name(&self) -> &str {
        "docs_read"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            self.name(),
            "Read one document.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    fn requires_permission(&self) -> bool {
        false
    }

    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        Ok(json!({ "content": "doc" }))
    }
}

fn agent(store: TraceStore) -> Agent {
    {
        let mut agent = Agent::new("docs-agent", "1.2.3", store);
        agent.description = "Answers documentation questions.".into();
        agent
            .models
            .push((ModelSelector::named("medium"), Arc::new(MockModel)));
        agent.default_model = Some(ModelSelector::named("medium"));
        agent.capabilities.push(Arc::new(ReadOnlyTool));
        agent.pricing = Pricing::new().with_tier("medium", 0.25, 1.25);
        agent.limits = AgentLimits::new()
            .with_max_model_calls(8)
            .with_max_cost_micro_usd(50_000);
        agent
    }
}

#[test]
fn describe_is_serde_stable() {
    let dir = tempdir();
    let agent = agent(TraceStore::new(dir.path()));

    let card = agent.describe();
    let card_json = serde_json::to_value(&card).unwrap();
    assert_eq!(
        card_json,
        json!({
            "name": "docs-agent",
            "version": "1.2.3",
            "description": "Answers documentation questions.",
            "tools": [
                {
                    "name": "docs_read",
                    "description": "Read one document.",
                    "privilege": "read_only",
                    "runs_in_background": false,
                    "schema": {
                        "name": "docs_read",
                        "description": "Read one document.",
                        "parameters": {
                            "type": "object",
                            "properties": { "path": { "type": "string" } },
                            "required": ["path"],
                            "additionalProperties": false
                        }
                    }
                },
                {
                    "name": "scratch_list",
                    "description": "List files and directories in your private scratch directory. Paths are relative to the scratch root; the default is the root itself.",
                    "privilege": "scratchpad",
                    "runs_in_background": false,
                    "schema": {
                        "name": "scratch_list",
                        "description": "List files and directories in your private scratch directory. Paths are relative to the scratch root; the default is the root itself.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "Relative directory path. Defaults to the scratch root." }
                            },
                            "additionalProperties": false
                        }
                    },
                    "scope": { "root": dir.path().join("scratch").display().to_string() }
                },
                {
                    "name": "scratch_read",
                    "description": "Read a UTF-8 text file from your private scratch directory. Paths are relative to the scratch root.",
                    "privilege": "scratchpad",
                    "runs_in_background": false,
                    "schema": {
                        "name": "scratch_read",
                        "description": "Read a UTF-8 text file from your private scratch directory. Paths are relative to the scratch root.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "File path relative to the scratch root." }
                            },
                            "required": ["path"],
                            "additionalProperties": false
                        }
                    },
                    "scope": { "root": dir.path().join("scratch").display().to_string() }
                },
                {
                    "name": "scratch_write",
                    "description": "Write text to a file in your private scratch directory, creating or overwriting it. Paths are relative to the scratch root; parent directories are created as needed.",
                    "privilege": "scratchpad",
                    "runs_in_background": false,
                    "schema": {
                        "name": "scratch_write",
                        "description": "Write text to a file in your private scratch directory, creating or overwriting it. Paths are relative to the scratch root; parent directories are created as needed.",
                        "parameters": {
                            "type": "object",
                            "properties": {
                                "path": { "type": "string", "description": "File path relative to the scratch root." },
                                "content": { "type": "string", "description": "The full contents to write." }
                            },
                            "required": ["path", "content"],
                            "additionalProperties": false
                        }
                    },
                    "scope": { "root": dir.path().join("scratch").display().to_string() }
                }
            ],
            "model_tiers": [
                {
                    "selector": "medium",
                    "default": true,
                    "pricing": {
                        "input_usd_per_m_tokens": 0.25,
                        "output_usd_per_m_tokens": 1.25
                    }
                }
            ],
            "limits": {
                "max_model_calls": 8,
                "max_cost_micro_usd": 50000
            }
        })
    );
}

#[tokio::test]
async fn traces_lists_header_lineage() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store);

    let root = agent.ask(Ask::new("root?")).await.unwrap();
    let child = agent
        .ask(Ask {
            trace_id: Some(root.trace_id.clone()),
            ..Ask::new("child?")
        })
        .await
        .unwrap();

    let traces = agent.traces().await.unwrap();
    assert_eq!(traces.len(), 2);
    assert!(
        traces
            .iter()
            .any(|trace| trace.trace_id == root.trace_id && trace.depends_on.is_none())
    );
    assert!(
        traces.iter().any(|trace| trace.trace_id == child.trace_id
            && trace.depends_on == Some(root.trace_id.clone()))
    );
}

struct TempDir {
    path: std::path::PathBuf,
}

impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn tempdir() -> TempDir {
    let mut path = std::env::temp_dir();
    let unique = format!(
        "huggr-agent-introspection-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    path.push(unique);
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
