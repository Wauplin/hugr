//! Concurrent asks (ROADMAP T3.2).
//!
//! Each [`Agent::ask`] is an independent session: it assembles its own engine,
//! prepares its own `.pending` scratch subtree (named by a monotonic per-agent
//! counter + pid), and persists a **new** immutable trace. Trace-store writes
//! reserve their id's path atomically (`create_new`), so N asks running in
//! parallel — a mix of fresh roots and forks off a shared parent — always
//! produce N distinct traces with correct lineage and never clobber each other.
//! Immutability makes forks race-free by design: the shared parent is only ever
//! read.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{Agent, Ask, STATUS_SUCCESS, TraceStore};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};

/// A scripted model with an effectively unbounded reply supply: it pops queued
/// replies but falls back to a constant once drained, so the pop order across
/// racing asks doesn't matter (these tests assert lineage, not message text).
struct MockModel {
    replies: Mutex<VecDeque<String>>,
}

impl MockModel {
    fn new<I: IntoIterator<Item = &'static str>>(replies: I) -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(replies.into_iter().map(String::from).collect()),
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
        let text = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "answer".to_string());
        sink.text(text.clone());
        Ok((ModelOutput::text(text), Usage::new(7, 3)))
    }
}

fn deterministic_clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn agent(store: TraceStore) -> Agent {
    let replies = vec!["answer"; 64];
    {
        let mut agent = Agent::new("test-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), MockModel::new(replies)));
        agent.system_prompt = Some("You answer tersely.".into());
        agent.clock = Some(deterministic_clock());
        agent
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn n_parallel_asks_mixed_fresh_and_forked_produce_n_valid_traces() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = Arc::new(agent(store.clone()));

    // A shared parent to fork from.
    let root = agent.ask(Ask::new("root question")).await.unwrap();

    // 16 concurrent asks: even indices are fresh roots, odd indices fork the
    // shared parent. They share one Agent (and one mock model behind a mutex).
    const N: usize = 16;
    let mut handles = Vec::new();
    for i in 0..N {
        let agent = agent.clone();
        let parent = root.trace_id.clone();
        handles.push(tokio::spawn(async move {
            let ask = if i % 2 == 0 {
                Ask::new(format!("fresh-{i}"))
            } else {
                Ask {
                    trace_id: Some(parent),
                    ..Ask::new(format!("fork-{i}"))
                }
            };
            (i, agent.ask(ask).await.unwrap())
        }));
    }

    let mut answers = Vec::new();
    for handle in handles {
        answers.push(handle.await.unwrap());
    }

    // Every ask succeeded and produced a distinct trace id (including the root).
    let mut ids = std::collections::BTreeSet::new();
    ids.insert(root.trace_id.clone());
    for (_, answer) in &answers {
        assert_eq!(answer.status, STATUS_SUCCESS);
        assert!(
            ids.insert(answer.trace_id.clone()),
            "each concurrent ask gets a distinct trace id — no collision"
        );
    }
    assert_eq!(ids.len(), N + 1, "root + N concurrent asks = N+1 traces");

    // Lineage is correct per ask: forks depend on the shared parent, fresh
    // asks are roots.
    for (i, answer) in &answers {
        let head = store.head(&answer.trace_id).unwrap();
        if i % 2 == 0 {
            assert_eq!(head.depends_on, None, "fresh ask {i} is a root");
        } else {
            assert_eq!(
                head.depends_on,
                Some(root.trace_id.clone()),
                "forked ask {i} depends on the shared parent"
            );
        }
    }

    // The store lists exactly N+1 traces and every one verifies bit-for-bit.
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), N + 1);
    for head in listed {
        hugr_replay::verify(&store.get(&head.trace_id).unwrap()).unwrap();
    }

    // The shared parent is still a valid, unchanged root.
    let parent_head = store.head(&root.trace_id).unwrap();
    assert_eq!(parent_head.depends_on, None);
}

// --- tiny tempdir helper (no external dev-dep) ---------------------------

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
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("hugr-agent-conc-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
