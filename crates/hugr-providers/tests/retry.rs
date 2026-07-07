//! Exercises [`OpenAiAdapter`]'s transport-level retry policy against a local
//! mock HTTP server (real reqwest path, no external network). The adapter must
//! retry transient failures (429, 5xx) with backoff and ultimately succeed,
//! while never retrying non-429 4xx semantic errors.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use hugr_core::{ContentPart, ContextBlock, ModelRequest, OpId, Role, SamplingParams};
use hugr_host::{ModelAdapter, ModelSink};
use hugr_providers::OpenAiAdapter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const SUCCESS_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\
data: [DONE]\n";

/// Spawn a mock HTTP server that handles connections in sequence. For each
/// connection, attempt `n` (1-based) returns the `n`-th status code in
/// `statuses`; once `statuses` is exhausted every further connection replies
/// `200` with a canned SSE body. Returns the base URL and a shared counter of
/// how many requests were actually received.
fn spawn_mock(statuses: Vec<u16>) -> (String, Arc<AtomicUsize>) {
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();

    // Bind synchronously so the URL is ready before the test sends a request.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");

    tokio::spawn(async move {
        let listener = TcpListener::from_std(listener).unwrap();
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => continue,
            };
            let n = counter_clone.fetch_add(1, Ordering::SeqCst);
            // Drain the request (the small JSON body fits in one read).
            let mut buf = [0u8; 8192];
            let _ = socket.read(&mut buf).await;

            let status = statuses.get(n).copied().unwrap_or(200);
            let response = if status == 200 {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{SUCCESS_BODY}"
                )
            } else {
                let body = format!("{{\"error\":\"status {status}\"}}");
                format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
            };
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });

    (url, counter)
}

fn user_request() -> ModelRequest {
    ModelRequest::new(
        vec![ContextBlock::new(
            Role::User,
            vec![ContentPart::Text("hi".to_string())],
        )],
        vec![],
        SamplingParams::new(),
    )
}

fn sink() -> ModelSink {
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    ModelSink::new(OpId(0), tx)
}

#[tokio::test]
async fn retries_transient_failures_then_succeeds() {
    // Fail twice (503, then 429), then succeed on the third attempt.
    let (base, hits) = spawn_mock(vec![503, 429]);

    let adapter = OpenAiAdapter::new("test-key", "gpt-test")
        .with_base_url(base)
        .with_max_attempts(4);

    let (output, _usage) = adapter
        .call(user_request(), &sink())
        .await
        .expect("transient failures should be retried until success");

    assert_eq!(output.text, "ok");
    assert_eq!(output.stop, "end_turn");
    // 2 failures + 1 success = 3 requests reached the server.
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gives_up_after_max_attempts() {
    // Always 500; with max_attempts = 3 the adapter should try exactly 3 times.
    let (base, hits) = spawn_mock(vec![500, 500, 500, 500, 500]);

    let adapter = OpenAiAdapter::new("test-key", "gpt-test")
        .with_base_url(base)
        .with_max_attempts(3);

    let err = adapter
        .call(user_request(), &sink())
        .await
        .expect_err("persistent 5xx should eventually give up");
    assert!(format!("{err:#}").contains("500"));
    assert_eq!(hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn does_not_retry_4xx_semantic_errors() {
    // A 400 is a semantic error: it must fail on the first try, no retry.
    let (base, hits) = spawn_mock(vec![400, 200, 200]);

    let adapter = OpenAiAdapter::new("test-key", "gpt-test")
        .with_base_url(base)
        .with_max_attempts(5);

    let err = adapter
        .call(user_request(), &sink())
        .await
        .expect_err("a 400 must not be retried");
    assert!(format!("{err:#}").contains("400"));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}
