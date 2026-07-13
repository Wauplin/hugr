//! Blob exchange end-to-end.
//!
//! Drives the real tokio [`Engine`] through [`Agent::ask`] with a scripted mock
//! model (same pattern as `scratchpad.rs`/`resume_fork.rs`), exercising the full
//! inbound → tool-read → outbound round-trip. Asserts the exit criteria:
//! - an orchestrator hands a file **in** (Bytes and Path), the agent reads it
//!   via `scratch_read`, and produces a file that comes back **out** as an
//!   `Answer.blob` with a `sha256` ref that resolves in the [`BlobStore`];
//! - identical outbound blobs dedupe to one stored object / one hash.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use huggr_agent::{Agent, Ask, AskError, BlobError, BlobHandle, BlobRef, TraceStore};
use huggr_core::{ModelOutput, ModelRequest, ModelSelector, ToolCall, Usage};
use huggr_host::{Clock, ModelAdapter, ModelSink};

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
    {
        let mut agent = Agent::new("blob-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), MockModel::new(outputs)));
        agent.system_prompt = Some("You process handed-in files.".into());
        agent.clock = Some(deterministic_clock());
        agent
    }
}

fn handle(blob_ref: BlobRef, media_type: &str, name: &str) -> BlobHandle {
    BlobHandle {
        blob_ref,
        media_type: media_type.to_string(),
        name: Some(name.to_string()),
    }
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
    let ask = Ask {
        blobs: vec![handle(
            BlobRef::Bytes {
                base64: BASE64.encode(payload),
            },
            "text/plain",
            "input.txt",
        )],
        ..Ask::new("process this")
    };

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
    assert_different_inode(
        &agent.blob_store().path_of(sha256),
        &dir.path()
            .join("scratch")
            .join(answer.trace_id.as_str())
            .join("out/report.md"),
    );
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

    let ask = Ask {
        blobs: vec![handle(
            BlobRef::Path {
                path: src.to_string_lossy().into_owned(),
            },
            "text/plain",
            "handed-in.txt",
        )],
        ..Ask::new("read the path blob")
    };

    let answer = agent.ask(ask).await.unwrap();
    let reads = tool_results(&store, &answer.trace_id, "scratch_read");
    assert_eq!(
        reads[0]["content"],
        serde_json::json!("local file contents")
    );
}

#[tokio::test]
async fn sha256_blob_hardlinks_into_scratch_when_filesystem_backed() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![read_call("c1", "shared.txt")]),
            ModelOutput::text("done"),
        ],
    );
    let stored = agent
        .blob_store()
        .put(b"shared bytes", "text/plain")
        .unwrap();

    let answer = agent
        .ask(Ask {
            blobs: vec![handle(
                BlobRef::Sha256 {
                    sha256: stored.hash.clone(),
                },
                "text/plain",
                "shared.txt",
            )],
            ..Ask::new("read shared")
        })
        .await
        .unwrap();

    let reads = tool_results(&store, &answer.trace_id, "scratch_read");
    assert_eq!(reads[0]["content"], serde_json::json!("shared bytes"));
    assert_same_inode(
        &agent.blob_store().path_of(&stored.hash),
        &dir.path()
            .join("scratch")
            .join(answer.trace_id.as_str())
            .join("shared.txt"),
    );
}

#[tokio::test]
async fn corrupted_sha256_blob_is_rejected_before_hardlinking() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store, vec![ModelOutput::text("must not run")]);
    let stored = agent
        .blob_store()
        .put(b"trusted bytes", "text/plain")
        .unwrap();
    let object = agent.blob_store().path_of(&stored.hash);
    make_writable(&object);
    std::fs::write(&object, b"corrupted bytes").unwrap();

    let result = agent
        .ask(Ask {
            blobs: vec![handle(
                BlobRef::Sha256 {
                    sha256: stored.hash.clone(),
                },
                "text/plain",
                "shared.txt",
            )],
            ..Ask::new("read shared")
        })
        .await;

    assert!(matches!(
        result,
        Err(AskError::Blob(BlobError::Store(
            huggr_replay::TraceError::InvalidBlobHash { hash }
        ))) if hash == stored.hash
    ));
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
    let count = count_files(&blobs_dir);
    assert_eq!(count, 2, "two distinct objects for three files (dedup)");
}

#[tokio::test]
async fn a_resumed_ask_does_not_re_emit_the_parents_outbound_blobs() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let parent_agent = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![
                write_call("c1", "notes/working.md", "keep me"),
                write_call("c2", "out/report.md", "deliverable"),
            ]),
            ModelOutput::text("done"),
        ],
    );
    let parent = parent_agent
        .ask(Ask::new("produce a report"))
        .await
        .unwrap();
    assert_eq!(parent.blobs.len(), 1);

    // The follow-up writes one new output; working state is inherited but the
    // parent's delivered `out/` files must not come back on this answer.
    let child_agent = agent(
        store.clone(),
        vec![
            ModelOutput::tool_calls(vec![
                read_call("c1", "notes/working.md"),
                write_call("c2", "out/second.md", "new deliverable"),
            ]),
            ModelOutput::text("done"),
        ],
    );
    let ask = Ask {
        trace_id: Some(parent.trace_id.clone()),
        ..Ask::new("follow up")
    };
    let child = child_agent.ask(ask).await.unwrap();

    let reads = tool_results(&store, &child.trace_id, "scratch_read");
    assert_eq!(reads[0]["content"], serde_json::json!("keep me"));
    assert_eq!(
        child.blobs.len(),
        1,
        "only this ask's own output is swept: {:?}",
        child.blobs
    );
    assert_eq!(child.blobs[0].name.as_deref(), Some("second.md"));
}

#[cfg(unix)]
fn assert_same_inode(a: &std::path::Path, b: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let a = std::fs::metadata(a).unwrap();
    let b = std::fs::metadata(b).unwrap();
    assert_eq!((a.dev(), a.ino()), (b.dev(), b.ino()));
}

#[cfg(unix)]
fn assert_different_inode(a: &std::path::Path, b: &std::path::Path) {
    use std::os::unix::fs::MetadataExt;
    let a = std::fs::metadata(a).unwrap();
    let b = std::fs::metadata(b).unwrap();
    assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
}

#[cfg(not(unix))]
fn assert_same_inode(_a: &std::path::Path, _b: &std::path::Path) {}

#[cfg(not(unix))]
fn assert_different_inode(_a: &std::path::Path, _b: &std::path::Path) {}

fn count_files(root: &std::path::Path) -> usize {
    let mut count = 0;
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            count += count_files(&path);
        } else if path.is_file() {
            count += 1;
        }
    }
    count
}

/// All tool results recorded under `name` in the trace stored at `id`.
fn tool_results(
    store: &TraceStore,
    id: &huggr_agent::TraceId,
    name: &str,
) -> Vec<serde_json::Value> {
    let trace = store.get(id).unwrap();
    trace
        .log
        .iter()
        .filter_map(|entry| match &entry.record {
            huggr_core::Record::ToolResult {
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

fn make_writable(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = meta.permissions();
        perms.set_mode(perms.mode() | 0o200);
        std::fs::set_permissions(path, perms).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut perms = meta.permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

fn tempdir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("huggr-blob-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
