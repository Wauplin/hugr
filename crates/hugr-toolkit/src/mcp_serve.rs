//! The MCP stdio server surface (ROADMAP T2.4, ARCHITECTURE §21.4).
//!
//! A cli-built agent binary run with `--mcp-serve` speaks the Model Context
//! Protocol over stdio: newline-delimited JSON-RPC 2.0. It advertises a single
//! `ask` tool (question + optional `trace_id` + blob handles) whose structured
//! result is the full [`Answer`]; server info comes from the [`AgentCard`]. This
//! is how Claude Code / other orchestrators consume a Hugr agent natively.
//!
//! Per the stateless 2026-07 MCP design (§21.4): session continuity rides our
//! `trace_id` in the tool arguments, **not** MCP session state — a follow-up is
//! just another `ask` carrying the previous answer's `trace_id`. We never use
//! MCP sampling; the agent owns its provider.

use hugr_agent::{Agent, AgentCard, Ask, BlobHandle, TraceId};
use hugr_plugin_abi::framing;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncWriteExt, BufReader};

/// The protocol version we advertise when the client doesn't pin one.
const DEFAULT_PROTOCOL_VERSION: &str = "2024-11-05";

/// Run the stdio MCP server loop against `agent` until stdin closes. Returns the
/// process exit code (0 on clean EOF). The agent is assembled once and reused
/// across `tools/call`s, so `trace_id` resume works within the session.
pub async fn serve(agent: &Agent, card: &AgentCard) -> i32 {
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
        let Some(response) = handle_message(agent, card, &message).await else {
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
        "tools/list" => ok(&id, json!({ "tools": [ask_tool_schema()] })),
        "ping" => ok(&id, json!({})),
        "tools/call" => match tools_call(agent, params).await {
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
}

/// Handle a `tools/call`: only `ask` is exposed. The [`Answer`] rides back as
/// `structuredContent` plus a text block; run failures are `status: "error"`
/// answers (not MCP `isError`), so orchestrators branch on the structured data.
async fn tools_call(agent: &Agent, params: Value) -> Result<Value, String> {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    if name != "ask" {
        return Err(format!("unknown tool: {name}"));
    }
    let args: AskArgs =
        serde_json::from_value(params.get("arguments").cloned().unwrap_or(Value::Null))
            .map_err(|e| format!("invalid `ask` arguments: {e}"))?;

    let mut ask = Ask::new(args.question).with_blobs(args.blobs);
    if let Some(id) = args.trace_id {
        ask = ask.with_trace_id(TraceId::new(id));
    }

    // Infra `AskError` (unknown parent id, store write) surfaces as an MCP error
    // result; run failures are already answers.
    let answer = agent.ask(ask).await.map_err(|e| e.to_string())?;
    let structured = serde_json::to_value(&answer).map_err(|e| e.to_string())?;
    Ok(json!({
        "content": [{ "type": "text", "text": answer.message }],
        "structuredContent": structured,
        "isError": false,
    }))
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
fn ask_tool_schema() -> Value {
    json!({
        "name": "ask",
        "description": "Ask this Hugr subagent a question. Pass the previous answer's trace_id to resume/fork the thread.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The question to ask." },
                "trace_id": { "type": "string", "description": "Resume/fork from this stored trace id." },
                "blobs": {
                    "type": "array",
                    "description": "Inbound file handles (contract BlobHandle JSON).",
                    "items": { "type": "object" }
                }
            },
            "required": ["question"]
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
        let schema = ask_tool_schema();
        assert_eq!(schema["name"], "ask");
        assert_eq!(schema["inputSchema"]["required"][0], "question");
        assert!(
            schema["inputSchema"]["properties"]
                .get("trace_id")
                .is_some()
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

        let home = std::env::temp_dir().join(format!("hugr-mcp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join("hugr.toml"),
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

        // tools/list advertises exactly the `ask` tool.
        let list = handle_message(&agent, &card, &json!({ "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        assert_eq!(list["result"]["tools"][0]["name"], "ask");

        // tools/call ask → structured Answer with a persisted trace_id.
        let call = handle_message(
            &agent,
            &card,
            &json!({ "id": 3, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "hi" } } }),
        )
        .await
        .unwrap();
        assert_eq!(call["result"]["isError"], false);
        let answer = &call["result"]["structuredContent"];
        let trace_id = answer["trace_id"].as_str().unwrap().to_string();
        assert!(!trace_id.is_empty(), "trace persisted: {answer}");

        // Resume by that trace_id → a new child trace (id round-trips across calls).
        let resume = handle_message(
            &agent,
            &card,
            &json!({ "id": 4, "method": "tools/call",
                     "params": { "name": "ask", "arguments": { "question": "again", "trace_id": trace_id } } }),
        )
        .await
        .unwrap();
        let child_id = resume["result"]["structuredContent"]["trace_id"]
            .as_str()
            .unwrap();
        assert_ne!(child_id, trace_id, "resume wrote a new child trace");

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
