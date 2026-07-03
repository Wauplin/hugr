//! Blob exchange with permissions end-to-end (ROADMAP T0.5, ARCHITECTURE §18.3).
//!
//! Drives the real tokio [`Engine`] through [`Agent::ask`] with a scripted mock
//! model (same pattern as `scratchpad.rs`/`resume_fork.rs`), exercising the full
//! inbound → tool-read → outbound round-trip. Asserts the exit criteria:
//! - an orchestrator hands a file **in** (Bytes and Path), the agent reads it
//!   via `scratch_read`, and produces a file that comes back **out** as an
//!   `Answer.blob` with a `sha256` ref that resolves in the [`BlobStore`];
//! - materialized inbound perms are applied (mode bits, unix);
//! - identical outbound blobs dedupe to one stored object / one hash.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use hugr_agent::{Agent, Ask, BlobHandle, BlobPerms, BlobRef, TraceStore};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, ToolCall, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};

/// A scripted model: each call pops the next queued [`ModelOutput`].
struct MockModel {
    outputs: Mutex<VecDeque<ModelOutput>>,
}

impl MockModel {
    fn new<I: IntoIterator<Item = ModelOutput>>(outputs: I) -> Arc<Self> {
        Arc::new(Self {
            outputs: Mutex::new(outputs.into_iter().collect()),
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
            .outputs
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted outputs"))?;
        if !output.text.is_empty() {
            sink.text(output.text.clone());
        }
        Ok((output, Usage::new(1, 1)))
    }
}

fn deterministic_clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn agent(store: TraceStore, outputs: Vec<ModelOutput>) -> Agent {
    Agent::builder("blob-agent", "0.1.0", store)
        .model(ModelSelector::named("medium"), MockModel::new(outputs))
        .system_prompt("You process handed-in files.")
        .clock(deterministic_clock())
        .build()
}

fn read_call(id: &str, path: &str) -> ToolCall {
    ToolCall::new(id, "scratch_read", serde_json::json!({ "path": path }))
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    ToolCall::new(
        id,
        "scratch_write",
        serde_json::json!({ "path": path, "content": content }),
    )
}

#[tokio::test]
async fn file_handed_in_as_bytes_is_read_then_produced_out_as_a_sha256_blob() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            // Read the handed-in file, then write a produced file into `out/`.
            ModelOutput::tool_calls(vec![
                read_call("c1", "input.txt"),
                write_call("c2", "out/report.md", "# derived from input"),
            ]),
            ModelOutput::text("done"),
        ],
    );

    let payload = b"hello from the orchestrator";
    let ask = Ask::new("process this").with_blobs(vec![
        BlobHandle::new(
            BlobRef::Bytes {
                base64: BASE64.encode(payload),
            },
            "text/plain",
        )
        .with_name("input.txt"),
    ]);

    let answer = agent.ask(ask).await.unwrap();

    // Inbound: the agent read the handed-in bytes via scratch_read.
    let reads = tool_results(&store, &answer.trace_id, "scratch_read");
    assert_eq!(
        reads[0]["content"],
        serde_json::json!("hello from the orchestrator")
    );

    // Outbound: exactly one produced blob, a sha256 ref that resolves.
    assert_eq!(answer.blobs.len(), 1, "one produced file swept from out/");
    let out = &answer.blobs[0];
    assert_eq!(out.name.as_deref(), Some("report.md"));
    assert_eq!(out.media_type, "text/markdown");
    let BlobRef::Sha256 { sha256 } = &out.blob_ref else {
        panic!("outbound blob must be a sha256 ref, got {:?}", out.blob_ref);
    };
    let bytes = agent.blob_store().get(sha256).unwrap();
    assert_eq!(bytes, b"# derived from input");
}

#[tokio::test]
async fn file_handed_in_as_path_is_materialized_and_read() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());

    // An orchestrator-local file handed in by path.
    let src = dir.path().join("orchestrator-local.txt");
    std::fs::write(&src, b"local file contents").unwrap();

    let agent = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![read_call("c1", "handed-in.txt")]),
            ModelOutput::text("done"),
        ],
    );

    let ask = Ask::new("read the path blob").with_blobs(vec![
        BlobHandle::new(
            BlobRef::Path {
                path: src.to_string_lossy().into_owned(),
            },
            "text/plain",
        )
        .with_name("handed-in.txt"),
    ]);

    let answer = agent.ask(ask).await.unwrap();
    let reads = tool_results(&store, &answer.trace_id, "scratch_read");
    assert_eq!(
        reads[0]["content"],
        serde_json::json!("local file contents")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn inbound_perms_are_applied_as_mode_bits() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            // List so the ask finalizes the scratch subtree under the trace id,
            // where we can then inspect the materialized files' mode bits.
            ModelOutput::tool_calls(vec![ToolCall::new(
                "c1",
                "scratch_list",
                serde_json::json!({}),
            )]),
            ModelOutput::text("done"),
        ],
    );

    let ask = Ask::new("perms").with_blobs(vec![
        // read-only (the default).
        BlobHandle::new(
            BlobRef::Bytes {
                base64: BASE64.encode(b"ro"),
            },
            "text/plain",
        )
        .with_name("ro.txt"),
        // read+write+execute.
        BlobHandle::new(
            BlobRef::Bytes {
                base64: BASE64.encode(b"rwx"),
            },
            "text/plain",
        )
        .with_perms(BlobPerms::new(true, true, true))
        .with_name("rwx.txt"),
    ]);

    let answer = agent.ask(ask).await.unwrap();

    // The finalized scratch subtree lives at <scratch_root>/<trace_id>.
    let scratch = dir.path().join(".scratch").join(answer.trace_id.as_str());
    let ro = std::fs::metadata(scratch.join("ro.txt")).unwrap();
    let rwx = std::fs::metadata(scratch.join("rwx.txt")).unwrap();
    assert_eq!(ro.permissions().mode() & 0o777, 0o400, "read-only → 0o400");
    assert_eq!(rwx.permissions().mode() & 0o777, 0o700, "rwx → 0o700");
}

#[tokio::test]
async fn identical_outbound_blobs_dedupe_by_hash() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            // Two produced files with identical content, plus a distinct one.
            ModelOutput::tool_calls(vec![
                write_call("c1", "out/a.txt", "same bytes"),
                write_call("c2", "out/b.txt", "same bytes"),
                write_call("c3", "out/c.txt", "other bytes"),
            ]),
            ModelOutput::text("done"),
        ],
    );

    let answer = agent.ask(Ask::new("produce files")).await.unwrap();

    assert_eq!(answer.blobs.len(), 3, "three produced files, three handles");
    let hashes: Vec<&str> = answer
        .blobs
        .iter()
        .map(|b| match &b.blob_ref {
            BlobRef::Sha256 { sha256 } => sha256.as_str(),
            other => panic!("expected sha256 ref, got {other:?}"),
        })
        .collect();
    // a.txt and b.txt share content → identical hash; c.txt differs.
    assert_eq!(hashes[0], hashes[1], "identical content → same sha256");
    assert_ne!(hashes[0], hashes[2], "different content → different sha256");

    // And the deduped content is stored exactly once on disk.
    let blobs_dir = dir.path().join(".blobs");
    let count = std::fs::read_dir(&blobs_dir).unwrap().count();
    assert_eq!(count, 2, "two distinct objects for three files (dedup)");
}

// --- helpers ----------------------------------------------------------------

/// All tool results recorded under `name` in the trace stored at `id`.
fn tool_results(
    store: &TraceStore,
    id: &hugr_agent::TraceId,
    name: &str,
) -> Vec<serde_json::Value> {
    let trace = store.get(id).unwrap();
    trace
        .log
        .iter()
        .filter_map(|entry| match &entry.record {
            hugr_core::Record::ToolResult {
                name: n, result, ..
            } if n == name => Some(result.clone()),
            _ => None,
        })
        .collect()
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
        // Inbound files may be materialized read-only; restore write on the
        // whole tree before removal so cleanup never fails.
        restore_writable(&self.path);
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn restore_writable(path: &std::path::Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o700);
        let _ = std::fs::set_permissions(path, perms);
    }
    if meta.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                restore_writable(&entry.path());
            }
        }
    }
}

fn tempdir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("hugr-blob-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
