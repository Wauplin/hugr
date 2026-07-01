//! Phase 5 exit criterion: a **third-party plugin** — this separate crate,
//! built as its own binary, depending on nothing from Baton — adds a working
//! tool the agent can call, with **no recompile of the core**.
//!
//! Cargo hands the built binary's path to this test via `CARGO_BIN_EXE_*`, so we
//! drive the real plugin process over the real subprocess transport.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use baton_core::{ModelOutput, ModelRequest, ModelSelector, Record, ToolCall, Usage, Value};
use baton_host::policy::AllowAll;
use baton_host::{Engine, Frontend, ModelAdapter, ModelSink, SubprocessPlugin};

/// Path to the example plugin binary (built by cargo for this crate's tests).
const PLUGIN_BIN: &str = env!("CARGO_BIN_EXE_baton_example_plugin");

/// A scripted model that pops a queued response per call.
struct MockModel {
    responses: Mutex<VecDeque<ModelOutput>>,
}

impl MockModel {
    fn new(responses: impl IntoIterator<Item = ModelOutput>) -> Arc<Self> {
        Arc::new(Self {
            responses: Mutex::new(responses.into_iter().collect()),
        })
    }
}

#[async_trait]
impl ModelAdapter for MockModel {
    async fn call(
        &self,
        _request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let output = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of responses"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

/// A silent front-end (all `Frontend` methods default to no-ops).
struct Silent;
impl Frontend for Silent {}

/// The transport works directly: `describe` reports the plugin's tools and
/// `invoke` runs one (streaming a chunk, then the terminal result).
#[tokio::test]
async fn subprocess_transport_describe_and_invoke() {
    use baton_host::{PluginSink, PluginTransport};

    let plugin = SubprocessPlugin::new(PLUGIN_BIN);

    // describe: the plugin advertises `uppercase` and `reverse`.
    let tools = plugin.describe().await.expect("describe should succeed");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"uppercase"), "tools: {names:?}");
    assert!(names.contains(&"reverse"));

    // invoke: `uppercase` streams a progress chunk then returns the result.
    let chunks = Arc::new(Mutex::new(Vec::new()));
    let sink = {
        let chunks = chunks.clone();
        PluginSink::new(move |c| chunks.lock().unwrap().push(c))
    };
    let result = plugin
        .invoke("uppercase", serde_json::json!({ "text": "hello" }), &sink)
        .await
        .expect("uppercase should succeed");
    assert_eq!(result["text"], "HELLO");
    assert_eq!(
        chunks.lock().unwrap().as_slice(),
        &[serde_json::json!({ "progress": "uppercasing" })],
        "the streamed progress chunk was forwarded"
    );

    // An unknown tool is a semantic error (Err), not a crash.
    let err = plugin
        .invoke("nope", serde_json::json!({}), &PluginSink::null())
        .await
        .expect_err("unknown tool should be a semantic error");
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("unknown")
    );
}

/// The agent calls a plugin tool end-to-end through the real engine: the model
/// requests `uppercase`, the host runs the plugin subprocess, and the result
/// flows back into the turn loop — proving a third-party tool is callable.
#[tokio::test]
async fn agent_calls_plugin_tool_through_the_engine() {
    // Load the plugin's tools as capabilities.
    let plugin = Arc::new(SubprocessPlugin::new(PLUGIN_BIN));
    let caps = baton_host::plugins::load(plugin)
        .await
        .expect("load should describe the plugin");
    assert_eq!(caps.len(), 2, "uppercase + reverse");

    // Scripted turn: call `uppercase`, then answer.
    let model = MockModel::new([
        ModelOutput::tool_calls(vec![ToolCall::new(
            "call-1",
            "uppercase",
            serde_json::json!({ "text": "baton plugins work" }),
        )]),
        ModelOutput::text("The plugin upper-cased it."),
    ]);

    let mut builder = Engine::builder()
        .model(ModelSelector::named("big"), model)
        .policy(Arc::new(AllowAll))
        .frontend(Box::new(Silent));
    for cap in caps {
        builder = builder.capability(cap);
    }
    let mut engine = builder.build();

    engine.user_turn("shout my sentence".into()).await;

    // The plugin ran and its result is in the durable log, correlated by name.
    let tool_result = engine
        .brain()
        .state()
        .log()
        .iter()
        .find_map(|e| match &e.record {
            Record::ToolResult { name, result, .. } if name == "uppercase" => Some(result.clone()),
            _ => None,
        })
        .expect("the plugin tool result should be logged");
    assert_eq!(
        tool_result["text"], "BATON PLUGINS WORK",
        "the plugin computed the result: {tool_result}"
    );
}

/// Sanity: the plugin never links Baton — it is a real out-of-tree tool. (This
/// asserts the binary exists and is what we run; the crate manifest proves the
/// dependency claim: its only runtime dep is `serde_json`.)
#[test]
fn plugin_binary_is_standalone() {
    assert!(
        std::path::Path::new(PLUGIN_BIN).exists(),
        "cargo should have built the standalone plugin binary at {PLUGIN_BIN}"
    );
    let _ = Value::Null; // keep the baton-core import meaningful
}
