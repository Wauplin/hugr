//! The MCP stdio server surface.
//!
//! A cli-built agent binary run with `--mcp-serve` speaks the Model Context Protocol over stdio: newline-delimited JSON-RPC 2.0. It advertises a single `ask` tool (question + optional `trace_id` + blob handles) whose structured result is the full [`Answer`](huggr_agent::Answer); server info comes from the [`AgentCard`]. This is how Claude Code / other orchestrators consume a Huggr agent natively.
//!
//! Session continuity rides our `trace_id` in the tool arguments, **not** MCP session state — a follow-up is just another `ask` carrying the previous answer's `trace_id`. We never use MCP sampling; the agent owns its provider.

use std::collections::BTreeMap;

use huggr_agent::{Agent, AgentCard, Ask, BlobHandle, TraceId, validate_model_blobs};
use huggr_host::framing;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};

use crate::manifest::AgentDefinition;
use crate::runtime::{RuntimeOptions, build_agent_with_options};
use crate::runtime_args::{RuntimeValues, apply_runtime_values};

/// The protocol version we advertise when the client doesn't pin one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the stdio MCP server loop against `agent` until stdin closes. Returns the
/// process exit code (0 on clean EOF). The agent is assembled once and reused
/// across `tools/call`s, so `trace_id` resume works within the session.
pub async fn serve(agent: &Agent, card: &AgentCard) -> i32 {
    serve_with(ServeMode::Agent { agent, card }).await
}

/// Run the stdio MCP server against a definition. Unlike [`serve`], this can
/// apply runtime arguments per `ask` call before assembling the agent, which is
/// how a single docs binary can narrow its `fs_read` jail to a different folder
/// on each invocation.
pub async fn serve_definition(def: AgentDefinition) -> i32 {
    serve_definition_with_options(def, RuntimeOptions::default()).await
}

/// Run the stdio MCP server against a definition plus explicit runtime wiring.
pub async fn serve_definition_with_options(def: AgentDefinition, options: RuntimeOptions) -> i32 {
    serve_with(ServeMode::Definition {
        def: Box::new(def),
        options: Box::new(options),
    })
    .await
}

enum ServeMode<'a> {
    Agent {
        agent: &'a Agent,
        card: &'a AgentCard,
    },
    Definition {
        def: Box<AgentDefinition>,
        options: Box<RuntimeOptions>,
    },
}

async fn serve_with(mode: ServeMode<'_>) -> i32 {
    let stdin = BufReader::new(tokio::io::stdin());
    let mut lines = tokio::io::AsyncBufReadExt::lines(stdin);
    let mut stdout = tokio::io::stdout();

    loop {
        let message: Value = match framing::read_json_line(&mut lines).await {
            Ok(Some(msg)) => msg,
            Ok(None) => return 0, // clean EOF — orchestrator disconnected
            Err(err) => {
                eprintln!("mcp: framing error: {err}");
                return 1;
            }
        };

        // Notifications (no `id`) get no response — just observe and continue.
        let Some(response) = handle_message_for(&mode, &message).await else {
            continue;
        };

        if let Err(err) = framing::write_json_line(&mut stdout, &response).await {
            eprintln!("mcp: write error: {err}");
            return 1;
        }
        if stdout.flush().await.is_err() {
            return 1;
        }
    }
}

async fn handle_message_for(mode: &ServeMode<'_>, message: &Value) -> Option<Value> {
    match mode {
        ServeMode::Agent { agent, card } => handle_message(agent, card, message).await,
        ServeMode::Definition { def, options } => {
            handle_definition_message(def, options, message).await
        }
    }
}

/// Dispatch one JSON-RPC message to its response. Returns `None` for
/// notifications (no `id` → no reply). Kept transport-free so the protocol is
/// testable in-process without a stdio subprocess.
pub(crate) async fn handle_message(
    agent: &Agent,
    card: &AgentCard,
    message: &Value,
) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => ok(
            &id,
            initialize_result(&card.name, &card.version, &card.description, &params),
        ),
        "tools/list" => ok(
            &id,
            json!({ "tools": [ask_tool_schema(None), feedback_tool_schema()] }),
        ),
        "ping" => ok(&id, json!({})),
        "tools/call" => match tools_call(agent, params).await {
            Ok(result) => ok(&id, result),
            Err(err) => rpc_error(&id, -32602, &err),
        },
        other => rpc_error(&id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

async fn handle_definition_message(
    def: &AgentDefinition,
    options: &RuntimeOptions,
    message: &Value,
) -> Option<Value> {
    let id = message.get("id").cloned()?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);

    let response = match method {
        "initialize" => ok(
            &id,
            initialize_result(
                &def.agent.name,
                if def.agent.version.is_empty() {
                    "0.0.0"
                } else {
                    &def.agent.version
                },
                &def.agent.description,
                &params,
            ),
        ),
        "tools/list" => ok(
            &id,
            json!({ "tools": [ask_tool_schema(Some(def)), feedback_tool_schema()] }),
        ),
        "ping" => ok(&id, json!({})),
        "tools/call" => match tools_call_definition(def, options, params).await {
            Ok(result) => ok(&id, result),
            Err(err) => rpc_error(&id, -32602, &err),
        },
        other => rpc_error(&id, -32601, &format!("method not found: {other}")),
    };
    Some(response)
}

/// Arguments accepted by the `ask` tool.
#[derive(Deserialize)]
struct AskArgs {
    question: String,
    #[serde(default)]
    trace_id: Option<String>,
    #[serde(default)]
    blobs: Vec<BlobHandle>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(flatten)]
    runtime: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct FeedbackArgs {
    trace_id: String,
    #[serde(default)]
    payload: Value,
}

/// Validate model-facing `ask` arguments into an [`Ask`]. The MCP client is an
/// untrusted caller: `Path` blob refs are rejected outright (no readable roots
/// exist for an external caller) and content addresses must be well formed.
fn ask_from_args(args: AskArgs) -> Result<Ask, String> {
    validate_model_blobs(&args.blobs, &[]).map_err(|e| format!("invalid `ask` blobs: {e}"))?;
    let trace_id = args
        .trace_id
        .map(TraceId::try_new)
        .transpose()
        .map_err(|e| format!("invalid `trace_id`: {e}"))?;
    Ok(Ask {
        question: args.question,
        trace_id,
        blobs: args.blobs,
        skills: args.skills,
        ..Ask::default()
    })
}

/// Handle a `tools/call`: only `ask` is exposed. The [`Answer`] rides back as
/// `structuredContent` plus a text block; run failures are `status: "error"`
/// answers (not MCP `isError`), so orchestrators branch on the structured data.
async fn tools_call(agent: &Agent, params: Value) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "feedback" {
        let args: FeedbackArgs =
            serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid `feedback` arguments: {e}"))?;
        let feedback = agent
            .feedback(
                TraceId::try_new(args.trace_id).map_err(|e| format!("invalid `trace_id`: {e}"))?,
                args.payload,
            )
            .await
            .map_err(|e| e.to_string())?;
        let structured = serde_json::to_value(&feedback).map_err(|e| e.to_string())?;
        return Ok(json!({
            "content": [{ "type": "text", "text": "feedback recorded" }],
            "structuredContent": structured,
            "isError": false,
        }));
    }
    if name != "ask" {
        return Err(format!("unknown tool: {name}"));
    }
    let args: AskArgs =
        serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `ask` arguments: {e}"))?;
    let ask = ask_from_args(args)?;

    // Infra `AskError` (unknown parent id, store write) surfaces as an MCP error
    // result; run failures are already answers.
    let answer = agent.ask(ask).await.map_err(|e| e.to_string())?;
    let structured = serde_json::to_value(&answer).map_err(|e| e.to_string())?;
    let text = answer_text(&answer.response);
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    }))
}

async fn tools_call_definition(
    def: &AgentDefinition,
    options: &RuntimeOptions,
    params: Value,
) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name == "feedback" {
        let args: FeedbackArgs =
            serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null))
                .map_err(|e| format!("invalid `feedback` arguments: {e}"))?;
        let (agent, warnings) = build_agent_with_options(def, options)
            .await
            .map_err(|e| e.to_string())?;
        for warning in &warnings {
            eprintln!("warning: {warning}");
        }
        let feedback = agent
            .feedback(
                TraceId::try_new(args.trace_id).map_err(|e| format!("invalid `trace_id`: {e}"))?,
                args.payload,
            )
            .await
            .map_err(|e| e.to_string())?;
        let structured = serde_json::to_value(&feedback).map_err(|e| e.to_string())?;
        return Ok(json!({
            "content": [{ "type": "text", "text": "feedback recorded" }],
            "structuredContent": structured,
            "isError": false,
        }));
    }
    if name != "ask" {
        return Err(format!("unknown tool: {name}"));
    }
    let args: AskArgs =
        serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `ask` arguments: {e}"))?;
    let runtime = runtime_values_from_mcp(def, &args.runtime)?;
    let mut def = def.clone();
    apply_runtime_values(&mut def, &runtime).map_err(|e| e.to_string())?;
    let (agent, warnings) = build_agent_with_options(&def, options)
        .await
        .map_err(|e| e.to_string())?;
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let ask = ask_from_args(args)?;
    let answer = agent.ask(ask).await.map_err(|e| e.to_string())?;
    let structured = serde_json::to_value(&answer).map_err(|e| e.to_string())?;
    let text = answer_text(&answer.response);
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    }))
}

fn answer_text(response: &Value) -> String {
    if let Some(error) = response.get("error").and_then(Value::as_str) {
        return error.to_string();
    }
    if let Some(text) = response.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    if let Some(value) = response.get("response") {
        if let Some(text) = value.as_str() {
            return text.to_string();
        }
        if let Some(summary) = value.get("summary").and_then(Value::as_str) {
            return summary.to_string();
        }
        return serde_json::to_string(value).unwrap_or_default();
    }
    serde_json::to_string(response).unwrap_or_default()
}

fn runtime_values_from_mcp(
    def: &AgentDefinition,
    raw: &BTreeMap<String, Value>,
) -> Result<RuntimeValues, String> {
    let mut values = RuntimeValues::new();
    for arg in &def.runtime.args {
        if let Some(value) = raw.get(&arg.name) {
            let Some(value) = value.as_str() else {
                return Err(format!("runtime argument `{}` must be a string", arg.name));
            };
            values.insert(arg.name.clone(), value.to_string());
        }
    }
    Ok(values)
}

/// The `initialize` result: capabilities + server info from the card.
fn initialize_result(name: &str, version: &str, description: &str, params: &Value) -> Value {
    let protocol = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": protocol,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": name, "version": version },
        "instructions": description,
    })
}

/// The JSON schema advertised for the `ask` tool.
fn ask_tool_schema(def: Option<&AgentDefinition>) -> Value {
    let mut properties = serde_json::Map::from_iter([
        (
            "question".to_string(),
            json!({ "type": "string", "description": "The question to ask." }),
        ),
        (
            "trace_id".to_string(),
            json!({ "type": "string", "description": "Resume/fork from this stored trace id." }),
        ),
        (
            "blobs".to_string(),
            json!({
                "type": "array",
                "description": "Inbound file handles (contract BlobHandle JSON). Only `bytes` and well-formed `sha256` refs are accepted; `path` refs are rejected.",
                "items": { "type": "object" }
            }),
        ),
        (
            "skills".to_string(),
            json!({
                "type": "array",
                "description": "Local SKILL.md folder paths to add for this ask.",
                "items": { "type": "string" }
            }),
        ),
    ]);
    let mut required = vec!["question".to_string()];
    if let Some(def) = def {
        for arg in &def.runtime.args {
            properties.insert(
                arg.name.clone(),
                json!({
                    "type": "string",
                    "description": arg.help,
                }),
            );
            if arg.required {
                required.push(arg.name.clone());
            }
        }
    }
    json!({
        "name": "ask",
        "description": "Ask this huglet a question. Pass the previous answer's trace_id to resume/fork the thread.",
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required
        }
    })
}

fn feedback_tool_schema() -> Value {
    json!({
        "name": "feedback",
        "description": "Append opaque feedback for a previously returned Huggr trace_id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "trace_id": {
                    "type": "string",
                    "description": "Trace id returned by an earlier ask."
                },
                "payload": {
                    "description": "Opaque feedback payload."
                }
            },
            "required": ["trace_id"],
            "additionalProperties": false
        }
    })
}

fn ok(id: &Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_schema_requires_question_and_advertises_resume() {
        let schema = ask_tool_schema(None);
        assert_eq!(schema["name"], "ask");
        assert_eq!(schema["inputSchema"]["required"][0], "question");
        assert!(
            schema["inputSchema"]["properties"]
                .get("trace_id")
                .is_some()
        );
    }

    #[test]
    fn ask_schema_includes_runtime_args() {
        let def = AgentDefinition::parse(
            r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[runtime.args.docs_path]
target = "tools.fs_read.root"
required = true
help = "Docs root."
"#,
            "huggr.toml",
        )
        .unwrap();
        let schema = ask_tool_schema(Some(&def));
        assert_eq!(
            schema["inputSchema"]["properties"]["docs_path"]["description"],
            "Docs root."
        );
        assert_eq!(
            schema["inputSchema"]["required"],
            json!(["question", "docs_path"])
        );
    }

    #[test]
    fn initialize_echoes_client_protocol_and_card_info() {
        let params = json!({ "protocolVersion": "2025-06-18" });
        let result = initialize_result("demo", "1.0.0", "d", &params);
        assert_eq!(result["protocolVersion"], "2025-06-18");
        assert_eq!(result["serverInfo"]["name"], "demo");
        assert_eq!(result["capabilities"]["tools"], json!({}));
        // Falls back to the default when the client omits a version.
        assert_eq!(
            initialize_result("demo", "1.0.0", "d", &Value::Null)["protocolVersion"],
            DEFAULT_PROTOCOL_VERSION
        );
    }

    #[test]
    fn error_and_ok_envelopes_are_jsonrpc() {
        let id = json!(7);
        assert_eq!(ok(&id, json!({}))["jsonrpc"], "2.0");
        assert_eq!(ok(&id, json!({}))["id"], 7);
        let err = rpc_error(&id, -32601, "nope");
        assert_eq!(err["error"]["code"], -32601);
        assert_eq!(err["error"]["message"], "nope");
    }

    /// Drive the full protocol dispatch against a real (in-process) agent:
    /// initialize → tools/list → ask → resume by the returned trace_id. No
    /// stdio subprocess, no network (the ask fails at the model → error answer,
    /// but a trace persists and round-trips).
    #[tokio::test]
    async fn protocol_round_trips_ask_and_resume() {
        use crate::manifest::AgentDefinition;
        use crate::runtime::build_agent;

        let home = std::env::temp_dir().join(format!("huggr-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("Cargo.toml"),
            "[package]\nname = \"mcpsrv\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(
            home.join("huggr.toml"),
            "[agent]\nname = \"mcpsrv\"\nversion = \"0.2.0\"\n[models.medium]\nmodel = \"m\"\n",
        )
        .unwrap();
        let def = AgentDefinition::load(&home).unwrap();
        let (agent, _) = build_agent(&def).await.unwrap();
        let card = agent.describe();

        // A notification (no id) yields no response.
        assert!(
            handle_message(
                &agent,
                &card,
                &json!({ "method": "notifications/initialized" })
            )
            .await
            .is_none()
        );

        // initialize reports our server info.
        let init = handle_message(
            &agent,
            &card,
            &json!({ "id": 1, "method": "initialize", "params": {} }),
        )
        .await
        .unwrap();
        assert_eq!(init["result"]["serverInfo"]["name"], "mcpsrv");
        assert_eq!(init["result"]["serverInfo"]["version"], "0.2.0");

        // tools/list advertises `ask` plus the side-channel feedback tool.
        let list = handle_message(&agent, &card, &json!({ "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "ask");
        assert_eq!(list["result"]["tools"][1]["name"], "feedback");

        // tools/call ask → structured Answer with a persisted trace_id.
        let call = handle_message(
            &agent,
            &card,
            &json!({ "id": 3, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "hi" } } }),
        )
        .await
        .unwrap();
        assert!(!call["result"]["isError"].as_bool().unwrap_or(false));
        let answer = &call["result"]["structuredContent"];
        let trace_id = answer["trace_id"].as_str().unwrap().to_string();
        assert!(!trace_id.is_empty(), "trace persisted: {answer}");

        // Resume by that trace_id → a new child trace (id round-trips across calls).
        let resume = handle_message(
            &agent,
            &card,
            &json!({ "id": 4, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "again", "trace_id": trace_id.clone() } } }),
        )
        .await
        .unwrap();
        let child_id = resume["result"]["structuredContent"]["trace_id"]
            .as_str()
            .unwrap();
        assert_ne!(child_id, trace_id, "resume wrote a new child trace");

        // Feedback appends to the sidecar and returns structured feedback.
        let feedback = handle_message(
            &agent,
            &card,
            &json!({ "id": 45, "method": "tools/call",
                     "params": { "name": "feedback", "arguments": { "trace_id": trace_id, "payload": { "score": 1 } } } }),
        )
        .await
        .unwrap();
        assert_eq!(feedback["result"]["isError"], false);
        assert_eq!(
            feedback["result"]["structuredContent"]["payload"]["score"],
            1
        );

        // A model-facing `path` blob ref is rejected before the ask runs.
        let path_blob = handle_message(
            &agent,
            &card,
            &json!({ "id": 6, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "hi",
                         "blobs": [{ "ref": { "kind": "path", "path": "/etc/passwd" }, "media_type": "text/plain" }] } } }),
        )
        .await
        .unwrap();
        assert_eq!(path_blob["error"]["code"], -32602);

        // A crafted trace id is an error, not a panic or a filesystem touch.
        let bad_trace = handle_message(
            &agent,
            &card,
            &json!({ "id": 7, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "hi", "trace_id": "../outside" } } }),
        )
        .await
        .unwrap();
        assert_eq!(bad_trace["error"]["code"], -32602);

        // An unknown tool is a JSON-RPC error.
        let bad = handle_message(
            &agent,
            &card,
            &json!({ "id": 5, "method": "tools/call", "params": { "name": "nope", "arguments": {} } }),
        )
        .await
        .unwrap();
        assert_eq!(bad["error"]["code"], -32602);

        let _ = std::fs::remove_dir_all(&home);
    }
}
