//! OpenAI Chat Completions adapter with streaming.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use futures_util::StreamExt;
use hugr_core::{ContentPart, ModelOutput, ModelRequest, Role, ToolCall, Usage};
use hugr_host::{ModelAdapter, ModelSink};
use serde_json::{Value, json};

// Defaults target the Hugging Face router (an OpenAI-compatible endpoint). The
// `/v1` suffix is part of the base URL; the adapter appends `/chat/completions`.
// Point `HUGR_BASE_URL` at `https://api.openai.com/v1` to use OpenAI directly.
const DEFAULT_BASE_URL: &str = "https://router.huggingface.co/v1";

// 4 attempts = 1 initial try + up to 3 retries, with exponential backoff capped
// so a flaky network or a transient 429/5xx recovers without a long stall.
const DEFAULT_MAX_ATTEMPTS: u32 = 4;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

/// An adapter for the OpenAI Chat Completions API (or any compatible endpoint
/// via `HUGR_BASE_URL`).
pub struct OpenAiAdapter {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
    max_attempts: u32,
}

impl OpenAiAdapter {
    /// Create an adapter with an explicit key and concrete model id.
    pub fn new(api_key: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
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

    /// Set the maximum number of attempts per model call (initial try plus
    /// retries) for *transient* transport failures — network errors, HTTP 429,
    /// and 5xx (default [`DEFAULT_MAX_ATTEMPTS`]). A value of `0` or `1` disables
    /// retries. Non-429 4xx (semantic) errors are never retried.
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// The maximum number of attempts per model call (see
    /// [`with_max_attempts`](Self::with_max_attempts)).
    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
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

        // Always stream; `include_usage` puts usage in the final SSE chunk.
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(extra) = request.extra.as_object() {
            for (key, value) in extra {
                if !value.is_null() {
                    body[key] = value.clone();
                }
            }
        }
        body
    }

    /// Send the chat-completions request, retrying *transient* transport
    /// failures with exponential backoff (capped at [`RETRY_MAX_DELAY`]) up to
    /// [`Self::max_attempts`].
    ///
    /// Retried: connection/timeout errors, HTTP 429, and 5xx. Never retried:
    /// 4xx other than 429 — those are semantic errors that won't fix themselves.
    /// On a successful (2xx) response the streaming body is returned untouched;
    /// the stream itself is consumed once and not retried.
    async fn send_with_retry(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut attempt = 1;
        loop {
            let outcome = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(body)
                .send()
                .await;

            let err = match outcome {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    let err = anyhow::anyhow!("OpenAI returned {status}: {text}");
                    // 429 and 5xx are transient; other 4xx are semantic and final.
                    if !is_retriable_status(status) {
                        return Err(err);
                    }
                    err
                }
                // Transport-level failures (connect/timeout/reset) are transient.
                Err(e) => anyhow::Error::new(e).context("failed to send request to OpenAI"),
            };

            if attempt >= self.max_attempts {
                return Err(err.context(format!("giving up after {attempt} attempt(s)")));
            }
            sleep(backoff_delay(attempt)).await;
            attempt += 1;
        }
    }
}

/// Whether an HTTP status should be retried: `429 Too Many Requests` and any
/// `5xx`. All other 4xx are semantic errors and are never retried.
fn is_retriable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Exponential backoff for `attempt` (1-based), capped at [`RETRY_MAX_DELAY`].
fn backoff_delay(attempt: u32) -> Duration {
    let factor = 1u32.checked_shl(attempt - 1).unwrap_or(u32::MAX);
    RETRY_BASE_DELAY.saturating_mul(factor).min(RETRY_MAX_DELAY)
}

async fn sleep(dur: Duration) {
    tokio::time::sleep(dur).await;
}

#[async_trait]
impl ModelAdapter for OpenAiAdapter {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        let body = self.build_body(&request);
        let resp = self.send_with_retry(&body).await?;

        let mut acc = Accumulator {
            model: self.model.clone(),
            ..Accumulator::default()
        };
        consume_sse(resp.bytes_stream(), &mut acc, sink).await?;

        Ok(acc.finish())
    }
}

/// Drain an SSE byte stream into the accumulator, line by line.
///
/// Generic over the stream so tests can feed fixture chunks without HTTP; the
/// adapter passes `reqwest`'s `bytes_stream()`.
async fn consume_sse<S, B, E>(
    mut stream: S,
    acc: &mut Accumulator,
    sink: &ModelSink,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = Result<B, E>> + Unpin,
    B: AsRef<[u8]>,
    E: std::error::Error + Send + Sync + 'static,
{
    let mut buf: Vec<u8> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.context("error while streaming response")?;
        buf.extend_from_slice(bytes.as_ref());

        // SSE is newline-delimited; process every complete line. Scan with
        // a start offset and parse borrowed slices, then compact the buffer
        // once per network chunk (avoids an O(buffer) drain per line).
        let mut start = 0;
        while let Some(rel) = buf[start..].iter().position(|&b| b == b'\n') {
            let pos = start + rel;
            // Borrows for valid UTF-8; allocates only for a rare invalid line.
            let line = String::from_utf8_lossy(&buf[start..pos]);
            start = pos + 1;
            if ingest_sse_line(&line, acc, sink) {
                break; // [DONE]
            }
        }
        buf.drain(..start);
    }

    // Some servers close the stream without a trailing newline on the final
    // line — often the one carrying `usage` or the last `finish_reason`.
    // Parse the residual bytes through the same path so they aren't dropped.
    if !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        ingest_sse_line(&line, acc, sink);
    }

    Ok(())
}

/// Parse one SSE line, folding any `data:` payload into the accumulator.
/// Returns `true` for the `[DONE]` sentinel.
fn ingest_sse_line(line: &str, acc: &mut Accumulator, sink: &ModelSink) -> bool {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(data) = line.strip_prefix("data:") {
        let data = data.trim();
        if data == "[DONE]" {
            return true;
        }
        if let Ok(value) = serde_json::from_str::<Value>(data) {
            acc.ingest(&value, sink);
        }
    }
    false
}

/// Accumulates a streamed chat-completions response into a consolidated result.
#[derive(Default)]
struct Accumulator {
    text: String,
    reasoning: String,
    tool_calls: BTreeMap<u64, ToolAccum>,
    stop: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    /// Real cost (USD) read from the router's response, when it reports one. The
    /// HF router (and some OpenAI-compatible gateways) include this in the final
    /// `usage` chunk; when present we use it verbatim instead of guessing.
    reported_cost: Option<f64>,
    /// The concrete model id, so the table fallback can look up a price when the
    /// response carries no cost.
    model: String,
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
            self.input_tokens = usage
                .get("prompt_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            self.output_tokens = usage
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if let Some(cost) = extract_cost(usage) {
                self.reported_cost = Some(cost);
            }
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
                            // Args are consolidated internally; only the announce
                            // (id + name) streams as a live delta.
                            entry.args.push_str(args);
                        }
                    }
                    // Announce the tool call once its name is known. Guarantee a
                    // stable, non-empty id first: if the server streamed the name
                    // before (or never sends) the id, synthesize one from the index
                    // so live deltas, the consolidated `ToolCall`, and the brain's
                    // tool-result correlation all agree.
                    if !entry.announced && !entry.name.is_empty() {
                        if entry.id.is_empty() {
                            entry.id = format!("call_{index}");
                        }
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

    fn finish(self) -> (ModelOutput, Usage) {
        let mut calls = Vec::new();
        for (index, tool) in self.tool_calls.into_iter() {
            // Guarantee a stable, non-empty id even for a call the server never
            // gave one (or one never announced because its name never arrived):
            // the brain correlates tool results by this id, so an empty id would
            // silently break correlation.
            let id = if tool.id.is_empty() {
                format!("call_{index}")
            } else {
                tool.id
            };
            let args = if tool.args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&tool.args).unwrap_or_else(|_| json!({ "_raw": tool.args }))
            };
            calls.push(ToolCall::new(id, tool.name, args));
        }

        let reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        let stop = self.stop.unwrap_or_else(|| "end_turn".to_string());
        let output = ModelOutput::new(self.text, reasoning, calls, stop);

        // Prefer the router's real cost; only fall back to the static price
        // table when the response carried none.
        let usage = build_usage(
            self.input_tokens,
            self.output_tokens,
            self.reported_cost,
            &self.model,
        );
        (output, usage)
    }
}

/// Pull a USD cost out of a provider `usage` object. Different OpenAI-compatible
/// gateways spell it differently, so we accept the common shapes: a top-level
/// `cost`/`total_cost`, or a nested `cost_details.total_cost`. Returns `None`
/// when the response carries no cost at all.
fn extract_cost(usage: &Value) -> Option<f64> {
    usage
        .get("cost")
        .or_else(|| usage.get("total_cost"))
        .and_then(Value::as_f64)
        .or_else(|| {
            usage
                .get("cost_details")
                .and_then(|d| d.get("total_cost").or_else(|| d.get("cost")))
                .and_then(Value::as_f64)
        })
}

/// Build [`Usage`], stashing cost in its opaque `extra` as `{ "cost": <usd>,
/// "cost_source": "router" | "estimated" }`. The brain never reads this; only a
/// host metrics front-end does.
fn build_usage(input_tokens: u64, output_tokens: u64, reported: Option<f64>, model: &str) -> Usage {
    let usage = Usage::new(input_tokens, output_tokens);
    match reported {
        Some(cost) => usage.with_extra(json!({ "cost": cost, "cost_source": "router" })),
        // Unknown models get no cost rather than a wrong guess.
        None => match estimate_cost(input_tokens, output_tokens, model) {
            Some(cost) => usage.with_extra(json!({ "cost": cost, "cost_source": "estimated" })),
            None => usage,
        },
    }
}

/// Static fallback prices in USD **per million tokens** `(input, output)`, used
/// only when the router response omits cost. Deliberately tiny — real cost from
/// the provider is always preferred; this is a best-effort estimate.
fn table_price(model: &str) -> Option<(f64, f64)> {
    // Match on a normalized id so `:provider` routing suffixes don't defeat it.
    let id = model.split(':').next().unwrap_or(model);
    match id {
        "gpt-4o" => Some((2.50, 10.00)),
        "gpt-4o-mini" => Some((0.15, 0.60)),
        _ => None,
    }
}

/// Estimate USD cost from token counts and the static [`table_price`]. Returns
/// `None` for models absent from the table (no guess beats a wrong guess).
fn estimate_cost(input_tokens: u64, output_tokens: u64, model: &str) -> Option<f64> {
    let (in_price, out_price) = table_price(model)?;
    let cost = (input_tokens as f64 * in_price + output_tokens as f64 * out_price) / 1_000_000.0;
    Some(cost)
}

fn map_stop(finish_reason: &str) -> String {
    match finish_reason {
        "stop" => "end_turn".to_string(),
        "tool_calls" | "function_call" => "tool_use".to_string(),
        "length" => "max_tokens".to_string(),
        other => other.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_core::{ContextBlock, Event, ModelDelta, ModelRequest, Role, ToolSchema};
    use hugr_host::ModelSink;

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
        assert_eq!(body["tools"][0]["function"]["name"], "shell");
    }

    #[test]
    fn build_body_passes_provider_extras() {
        let mut request = ModelRequest::new(
            vec![ContextBlock::new(
                Role::User,
                vec![ContentPart::Text("answer".into())],
            )],
            Vec::new(),
        );
        request.extra = json!({
            "response_format": {
                "type": "json_schema",
                "json_schema": {
                    "name": "docs_response",
                    "strict": true,
                    "schema": { "type": "object" }
                }
            }
        });

        let body = adapter().build_body(&request);

        assert_eq!(
            body["response_format"]["json_schema"]["name"],
            "docs_response"
        );
        assert_eq!(
            body["response_format"]["json_schema"]["schema"],
            json!({ "type": "object" })
        );
    }

    #[test]
    fn with_model_overrides_model() {
        let adapter = OpenAiAdapter::new("test-key", "original").with_model("replacement");
        assert_eq!(adapter.model(), "replacement");
    }

    #[test]
    fn max_attempts_defaults_and_is_overridable() {
        let adapter = OpenAiAdapter::new("test-key", "gpt-test");
        assert_eq!(adapter.max_attempts(), DEFAULT_MAX_ATTEMPTS);
        // Override is honored; 0 is clamped to at least 1 (one attempt, no retry).
        assert_eq!(adapter.with_max_attempts(7).max_attempts(), 7);
        assert_eq!(
            OpenAiAdapter::new("test-key", "gpt-test")
                .with_max_attempts(0)
                .max_attempts(),
            1
        );
    }

    #[test]
    fn only_429_and_5xx_are_retriable() {
        use reqwest::StatusCode;
        assert!(is_retriable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retriable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retriable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retriable_status(StatusCode::SERVICE_UNAVAILABLE));
        // Non-429 4xx are semantic and must not be retried.
        assert!(!is_retriable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retriable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retriable_status(StatusCode::NOT_FOUND));
        assert!(!is_retriable_status(StatusCode::UNPROCESSABLE_ENTITY));
        // 2xx is success, not a retry.
        assert!(!is_retriable_status(StatusCode::OK));
    }

    #[test]
    fn backoff_grows_exponentially_and_caps() {
        assert_eq!(backoff_delay(1), RETRY_BASE_DELAY);
        assert_eq!(backoff_delay(2), RETRY_BASE_DELAY * 2);
        assert_eq!(backoff_delay(3), RETRY_BASE_DELAY * 4);
        // A large attempt count saturates at the cap rather than overflowing.
        assert_eq!(backoff_delay(100), RETRY_MAX_DELAY);
    }

    #[tokio::test]
    async fn accumulator_consolidates_text_and_usage() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

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

        let (output, usage) = acc.finish();
        assert_eq!(output.text, "Hello");
        assert!(output.tool_calls.is_empty());
        assert_eq!(output.stop, "end_turn");
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
    async fn cost_from_response_is_used_verbatim_without_a_table_guess() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

        // The router reports a real cost in the final usage chunk. Use a model
        // that *is* in the table so we can prove the table is NOT consulted.
        let mut acc = Accumulator {
            model: "gpt-4o".into(),
            ..Accumulator::default()
        };
        acc.ingest(
            &json!({ "choices": [{ "delta": { "content": "hi" } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [], "usage": {
                "prompt_tokens": 1000,
                "completion_tokens": 1000,
                "cost": 0.000123
            } }),
            &sink,
        );

        let (_output, usage) = acc.finish();
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 1000);
        // Cost comes straight from the response, tagged as router-sourced.
        assert_eq!(usage.extra["cost"], json!(0.000123));
        assert_eq!(usage.extra["cost_source"], json!("router"));
    }

    #[tokio::test]
    async fn cost_falls_back_to_table_when_response_omits_it() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

        // No cost in the response → estimate from the static table for gpt-4o:
        // 1M in @ $2.50 + 1M out @ $10.00 = $12.50.
        let mut acc = Accumulator {
            model: "gpt-4o".into(),
            ..Accumulator::default()
        };
        acc.ingest(
            &json!({ "choices": [], "usage": {
                "prompt_tokens": 1_000_000,
                "completion_tokens": 1_000_000
            } }),
            &sink,
        );

        let (_output, usage) = acc.finish();
        assert_eq!(usage.extra["cost"], json!(12.5));
        assert_eq!(usage.extra["cost_source"], json!("estimated"));
    }

    #[tokio::test]
    async fn unknown_model_without_reported_cost_has_no_cost() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

        // Not in the table and no cost in the response → no guess at all.
        let mut acc = Accumulator {
            model: "google/gemma-4-31B-it".into(),
            ..Accumulator::default()
        };
        acc.ingest(
            &json!({ "choices": [], "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20
            } }),
            &sink,
        );

        let (_output, usage) = acc.finish();
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 20);
        assert!(usage.extra.is_null(), "no cost should be guessed");
    }

    #[test]
    fn extract_cost_accepts_common_shapes() {
        assert_eq!(extract_cost(&json!({ "cost": 0.5 })), Some(0.5));
        assert_eq!(extract_cost(&json!({ "total_cost": 0.25 })), Some(0.25));
        assert_eq!(
            extract_cost(&json!({ "cost_details": { "total_cost": 0.75 } })),
            Some(0.75)
        );
        assert_eq!(extract_cost(&json!({ "prompt_tokens": 5 })), None);
    }

    #[tokio::test]
    async fn accumulator_assembles_streamed_tool_call() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

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

        let (output, _usage) = acc.finish();
        assert_eq!(output.stop, "tool_use");
        assert_eq!(output.tool_calls.len(), 1);
        let call = &output.tool_calls[0];
        assert_eq!(call.id, "call-9");
        assert_eq!(call.name, "shell");
        assert_eq!(call.args, json!({ "cmd": "ls" }));
    }

    // Regression: a server that closes the byte stream without a trailing
    // newline after the final `data:` line (here carrying both `finish_reason`
    // and the `usage` chunk) must not have that line silently dropped — that
    // would fold Usage to 0/0, lose the cost, and degrade the stop reason.
    #[tokio::test]
    async fn final_unterminated_sse_line_is_parsed() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

        let mut acc = Accumulator {
            model: "gpt-test".into(),
            ..Accumulator::default()
        };
        // Fixture: a normal newline-terminated delta, then a final line split
        // across two network chunks with NO trailing newline before EOF.
        let chunks: Vec<Result<Vec<u8>, std::convert::Infallible>> = vec![
            Ok(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_vec()),
            Ok(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}],".to_vec()),
            Ok(
                b"\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"cost\":0.000123}}"
                    .to_vec(),
            ),
        ];
        let stream = futures_util::stream::iter(chunks);
        consume_sse(stream, &mut acc, &sink).await.unwrap();

        let (output, usage) = acc.finish();
        assert_eq!(output.text, "hi");
        // `length` must survive as max_tokens, not degrade to the end_turn default.
        assert_eq!(output.stop, "max_tokens");
        // The usage chunk on the unterminated line must not fold to 0/0.
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 3);
        assert_eq!(usage.extra["cost"], json!(0.000123));
        assert_eq!(usage.extra["cost_source"], json!("router"));
    }

    // Regression: a (non-conforming) OpenAI-compatible server that streams a tool
    // call's `arguments` and `name` *before* the `id` — or never sends an id at
    // all — must not produce empty-id deltas or an empty-id consolidated call (the
    // brain correlates tool results by this id). A stable id is synthesized from
    // the index, and the pre-id args are buffered then flushed exactly once.
    #[tokio::test]
    async fn tool_call_with_no_id_gets_a_stable_synthesized_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let sink = ModelSink::new(hugr_core::OpId(0), tx);

        let mut acc = Accumulator::default();
        // args arrive first, with no id and no name yet
        acc.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "arguments": "{\"cmd\":" } }
            ] } }] }),
            &sink,
        );
        // then the name (still no id) — this is where the call is announced
        acc.ingest(
            &json!({ "choices": [{ "delta": { "tool_calls": [
                { "index": 0, "function": { "name": "shell", "arguments": "\"ls\"}" } }
            ] } }] }),
            &sink,
        );
        acc.ingest(
            &json!({ "choices": [{ "delta": {}, "finish_reason": "tool_calls" }] }),
            &sink,
        );

        let (output, _usage) = acc.finish();
        assert_eq!(output.tool_calls.len(), 1);
        let call = &output.tool_calls[0];
        assert_eq!(call.id, "call_0", "synthesized a stable id from the index");
        assert_eq!(call.name, "shell");
        assert_eq!(
            call.args,
            json!({ "cmd": "ls" }),
            "buffered args reassemble"
        );

        // No streamed announce may carry an empty id.
        drop(sink);
        while let Ok(Event::ModelDelta { delta, .. }) = rx.try_recv() {
            if let ModelDelta::ToolCallStart { id, .. } = delta {
                assert!(!id.is_empty());
            }
        }
    }
}
