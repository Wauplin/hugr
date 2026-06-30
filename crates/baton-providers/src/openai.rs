//! OpenAI Chat Completions adapter with streaming.
//!
//! Translates the canonical [`ModelRequest`] into the chat-completions wire
//! format, streams the SSE response, forwards deltas through the
//! [`ModelSink`], and returns the consolidated [`ModelOutput`] + [`Usage`].

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use async_trait::async_trait;
use baton_core::{ContentPart, ModelOutput, ModelRequest, Role, StopReason, ToolCall, Usage};
use baton_host::{ModelAdapter, ModelSink};
use futures_util::StreamExt;
use serde_json::{Value, json};

// Defaults target the Hugging Face router (an OpenAI-compatible endpoint). The
// `/v1` suffix is part of the base URL; the adapter appends `/chat/completions`.
// Point `OPENAI_BASE_URL` at `https://api.openai.com/v1` to use OpenAI directly.
const DEFAULT_BASE_URL: &str = "https://router.huggingface.co/v1";
const DEFAULT_MODEL: &str = "google/gemma-4-31B-it:together";

/// An adapter for the OpenAI Chat Completions API (or any compatible endpoint
/// via `OPENAI_BASE_URL`).
pub struct OpenAiAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiAdapter {
    /// Create an adapter with an explicit key and concrete model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }

    /// Build from the environment:
    ///
    /// - **API key:** `OPENAI_API_KEY`, else `HF_TOKEN`, else the output of
    ///   `hf auth token` if the `hf` CLI is installed and logged in.
    /// - **Model:** `OPENAI_MODEL` (default `google/gemma-4-31B-it:together`).
    /// - **Base URL:** `OPENAI_BASE_URL` (default the Hugging Face router).
    pub fn from_env() -> anyhow::Result<Self> {
        let api_key = resolve_api_key().context(
            "no API key found: set OPENAI_API_KEY or HF_TOKEN, or log in with `hf auth login`",
        )?;
        let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        Ok(Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            base_url,
        })
    }

    /// Override the base URL (for Azure / OpenAI-compatible gateways).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Override the concrete model id (e.g. from a CLI flag).
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// The concrete model id this adapter calls.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The base URL this adapter posts to.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn build_body(&self, request: &ModelRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();

        for block in &request.blocks {
            match block.role {
                Role::System => messages.push(json!({
                    "role": "system",
                    "content": collect_text(&block.content),
                })),
                Role::User => messages.push(json!({
                    "role": "user",
                    "content": collect_text(&block.content),
                })),
                Role::Assistant => {
                    let mut text = String::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    for part in &block.content {
                        match part {
                            ContentPart::Text(t) => text.push_str(t),
                            ContentPart::ToolUse { id, name, args } => tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(args)
                                        .unwrap_or_else(|_| "{}".to_string()),
                                },
                            })),
                            _ => {}
                        }
                    }
                    let mut msg = json!({ "role": "assistant" });
                    // OpenAI allows null content when tool_calls are present.
                    msg["content"] = if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    };
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = Value::Array(tool_calls);
                    }
                    messages.push(msg);
                }
                Role::Tool => {
                    for part in &block.content {
                        if let ContentPart::ToolResult { id, result } = part {
                            messages.push(json!({
                                "role": "tool",
                                "tool_call_id": id,
                                "content": stringify(result),
                            }));
                        }
                    }
                }
                // Forward-compatible: skip roles this adapter doesn't map.
                _ => {}
            }
        }

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    },
                })
            })
            .collect();

        // Streaming is the only mode (see `ModelAdapter`): always request a
        // streamed response, and ask for usage in the final SSE chunk.
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(t) = request.params.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = request.params.max_tokens {
            body["max_tokens"] = json!(m);
        }
        body
    }
}

#[async_trait]
impl ModelAdapter for OpenAiAdapter {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let body = self.build_body(&request);
        let resp = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("failed to send request to OpenAI")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("OpenAI returned {status}: {text}");
        }

        let mut acc = Accumulator::default();
        let mut buf: Vec<u8> = Vec::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.context("error while streaming response")?;
            buf.extend_from_slice(&bytes);

            // SSE is newline-delimited; process every complete line.
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches(['\r', '\n']);
                if let Some(data) = line.strip_prefix("data:") {
                    let data = data.trim();
                    if data == "[DONE]" {
                        break;
                    }
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        acc.ingest(&value, sink);
                    }
                }
            }
        }

        Ok(acc.finish(sink))
    }
}

/// Accumulates a streamed chat-completions response into a consolidated result.
#[derive(Default)]
struct Accumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u64, ToolAccum>,
    stop: Option<StopReason>,
    usage: Usage,
}

#[derive(Default)]
struct ToolAccum {
    id: String,
    name: String,
    args: String,
    announced: bool,
}

impl Accumulator {
    fn ingest(&mut self, value: &Value, sink: &ModelSink) {
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            let input = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let output = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.usage = Usage::new(input, output);
        }

        let Some(choice) = value.get("choices").and_then(|c| c.get(0)) else {
            return;
        };

        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    sink.text(content);
                    self.text.push_str(content);
                }
            }
            // Some OpenAI-compatible models stream reasoning separately.
            if let Some(reasoning) = delta
                .get("reasoning_content")
                .or_else(|| delta.get("reasoning"))
                .and_then(Value::as_str)
            {
                if !reasoning.is_empty() {
                    sink.reasoning(reasoning);
                    self.reasoning.push_str(reasoning);
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tc in tool_calls {
                    let index = tc.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = self.tool_calls.entry(index).or_default();
                    if let Some(id) = tc.get("id").and_then(Value::as_str) {
                        if !id.is_empty() {
                            entry.id = id.to_string();
                        }
                    }
                    if let Some(function) = tc.get("function") {
                        if let Some(name) = function.get("name").and_then(Value::as_str) {
                            if !name.is_empty() {
                                entry.name = name.to_string();
                            }
                        }
                        if let Some(args) = function.get("arguments").and_then(Value::as_str) {
                            entry.args.push_str(args);
                            if !args.is_empty() {
                                sink.tool_call_args(&entry.id, args);
                            }
                        }
                    }
                    // Announce the tool call once its name is known.
                    if !entry.announced && !entry.name.is_empty() {
                        entry.announced = true;
                        sink.tool_call_start(&entry.id, &entry.name);
                    }
                }
            }
        }

        if let Some(finish) = choice.get("finish_reason").and_then(Value::as_str) {
            self.stop = Some(map_stop(finish));
        }
    }

    fn finish(self, sink: &ModelSink) -> (ModelOutput, Usage) {
        let mut calls = Vec::new();
        for tool in self.tool_calls.into_values() {
            sink.tool_call_end(&tool.id);
            let args = if tool.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&tool.args).unwrap_or_else(|_| json!({ "_raw": tool.args }))
            };
            calls.push(ToolCall::new(tool.id, tool.name, args));
        }

        let reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        let stop = self.stop.unwrap_or(StopReason::EndTurn);
        let output = ModelOutput::new(self.text, reasoning, calls, stop);
        (output, self.usage)
    }
}

fn map_stop(finish_reason: &str) -> StopReason {
    match finish_reason {
        "stop" => StopReason::EndTurn,
        "tool_calls" | "function_call" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        other => StopReason::Other(other.to_string()),
    }
}

fn collect_text(content: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in content {
        if let ContentPart::Text(t) = part {
            out.push_str(t);
        }
    }
    out
}

/// Render an opaque tool-result value to the string OpenAI expects as `content`.
fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Resolve an API key from, in order: `OPENAI_API_KEY`, `HF_TOKEN`, then the
/// `hf` CLI's stored token. Returns `None` if none are available.
fn resolve_api_key() -> Option<String> {
    for var in ["OPENAI_API_KEY", "HF_TOKEN"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    hf_cli_token()
}

/// The token stored by the `hf` CLI (`hf auth token`), if it is installed and
/// logged in.
fn hf_cli_token() -> Option<String> {
    let output = std::process::Command::new("hf")
        .args(["auth", "token"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use baton_core::{ContextBlock, ModelRequest, Role, SamplingParams, ToolSchema};
    use baton_host::ModelSink;

    fn adapter() -> OpenAiAdapter {
        OpenAiAdapter::new("test-key", "gpt-test")
    }

    #[test]
    fn build_body_maps_roles_tools_and_tool_calls() {
        let request = ModelRequest::new(
            vec![
                ContextBlock::new(Role::System, vec![ContentPart::Text("be helpful".into())]),
                ContextBlock::new(Role::User, vec![ContentPart::Text("list files".into())]),
                ContextBlock::new(
                    Role::Assistant,
                    vec![ContentPart::ToolUse {
                        id: "call-1".into(),
                        name: "shell".into(),
                        args: json!({ "cmd": "ls" }),
                    }],
                ),
                ContextBlock::new(
                    Role::Tool,
                    vec![ContentPart::ToolResult {
                        id: "call-1".into(),
                        result: json!({ "stdout": "a.txt" }),
                    }],
                ),
            ],
            vec![ToolSchema::new(
                "shell",
                "run a command",
                json!({ "type": "object" }),
            )],
            SamplingParams::new()
                .with_temperature(0.5)
                .with_max_tokens(128),
        );

        let body = adapter().build_body(&request);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");

        // Assistant message carries a tool_call whose id matches the tool result.
        let assistant = &messages[2];
        assert_eq!(assistant["role"], "assistant");
        assert!(assistant["content"].is_null());
        let call = &assistant["tool_calls"][0];
        assert_eq!(call["id"], "call-1");
        assert_eq!(call["function"]["name"], "shell");
        // Arguments are serialized as a JSON *string*.
        assert_eq!(call["function"]["arguments"], json!("{\"cmd\":\"ls\"}"));

        // Tool result references the same id.
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call-1");

        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["stream"], true);
        assert_eq!(body["temperature"], json!(0.5));
        assert_eq!(body["max_tokens"], json!(128));
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
    }

    #[test]
    fn with_model_overrides_model() {
        let adapter = OpenAiAdapter::new("test-key", "original").with_model("replacement");
        assert_eq!(adapter.model(), "replacement");
    }

    #[tokio::test]
    async fn accumulator_consolidates_text_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(baton_core::OpId(0), tx);

        let mut acc = Accumulator::default();
        acc.ingest(
            &json!({ "choices": [{ "delta": { "content": "Hel" } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [{ "delta": { "content": "lo" } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [], "usage": { "prompt_tokens": 7, "completion_tokens": 3 } }),
            &sink,
        );

        let (output, usage) = acc.finish(&sink);
        assert_eq!(output.text, "Hello");
        assert!(output.tool_calls.is_empty());
        assert_eq!(output.stop, StopReason::EndTurn);
        assert_eq!(usage, Usage::new(7, 3));

        // Streamed deltas were forwarded to the sink.
        drop(sink);
        let mut deltas = 0;
        while rx.recv().await.is_some() {
            deltas += 1;
        }
        assert!(deltas >= 2, "expected streamed text deltas");
    }

    #[tokio::test]
    async fn accumulator_assembles_streamed_tool_call() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(baton_core::OpId(0), tx);

        let mut acc = Accumulator::default();
        // Tool call arrives in fragments across several chunks.
        acc.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "id": "call-9", "function": { "name": "shell", "arguments": "{\"cmd\":" } }
            ] } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "\"ls\"}" } }
            ] } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            &sink,
        );

        let (output, _usage) = acc.finish(&sink);
        assert_eq!(output.stop, StopReason::ToolUse);
        assert_eq!(output.tool_calls.len(), 1);
        let call = &output.tool_calls[0];
        assert_eq!(call.id, "call-9");
        assert_eq!(call.name, "shell");
        assert_eq!(call.args, json!({ "cmd": "ls" }));
    }
}
