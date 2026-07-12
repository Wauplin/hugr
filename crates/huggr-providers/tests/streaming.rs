//! Drives [`OpenAiAdapter`] against a local mock SSE server, exercising the real
//! reqwest streaming + SSE-parsing path end-to-end (no external network).

use huggr_core::{ContentPart, ContextBlock, ModelRequest, OpId, Role};
use huggr_host::{ModelAdapter, ModelSink};
use huggr_providers::OpenAiAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Spawn a one-shot HTTP server that replies with a canned SSE body, and return
/// its `http://host:port` base URL.
async fn spawn_mock_sse(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Drain the request (small JSON body fits in one read).
        let mut buf = [0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        socket.shutdown().await.ok();
    });
    format!("http://{addr}")
}

fn user_request(text: &str) -> ModelRequest {
    ModelRequest::new(
        vec![ContextBlock::new(
            Role::User,
            vec![ContentPart::Text(text.to_string())],
        )],
        vec![],
    )
}

#[tokio::test]
async fn streams_text_and_usage_from_server() {
    let body = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\
data: {\"choices\":[{\"delta\":{\"content\":\", world\"}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":4}}\n\
data: [DONE]\n";
    let base = spawn_mock_sse(body).await;

    let adapter = OpenAiAdapter::new("test-key", "gpt-test").with_base_url(base);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = ModelSink::new(OpId(0), tx);

    let (output, usage) = adapter
        .call(user_request("hi"), &sink)
        .await
        .expect("adapter call should succeed");

    assert_eq!(output.text, "Hello, world");
    assert_eq!(output.stop, "end_turn");
    assert!(output.tool_calls.is_empty());
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 4);

    // The text was streamed as deltas, not just returned at the end.
    drop(sink);
    let mut streamed = String::new();
    while let Some(event) = rx.recv().await {
        if let huggr_core::Event::ModelDelta {
            delta: huggr_core::ModelDelta::Text(t),
            ..
        } = event
        {
            streamed.push_str(&t);
        }
    }
    assert_eq!(streamed, "Hello, world");
}

#[tokio::test]
async fn streams_a_tool_call_from_server() {
    let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-7\",\"function\":{\"name\":\"shell\",\"arguments\":\"{\\\"cmd\\\":\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"ls\\\"}\"}}]}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\
data: [DONE]\n";
    let base = spawn_mock_sse(body).await;

    let adapter = OpenAiAdapter::new("test-key", "gpt-test").with_base_url(base);
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = ModelSink::new(OpId(0), tx);

    let (output, _usage) = adapter
        .call(user_request("list files"), &sink)
        .await
        .expect("adapter call should succeed");

    assert_eq!(output.stop, "tool_use");
    assert_eq!(output.tool_calls.len(), 1);
    let call = &output.tool_calls[0];
    assert_eq!(call.id, "call-7");
    assert_eq!(call.name, "shell");
    assert_eq!(call.args, serde_json::json!({ "cmd": "ls" }));
}
