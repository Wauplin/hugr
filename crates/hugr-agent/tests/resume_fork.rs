//! Resume & fork semantics end-to-end.
//!
//! Drives the real tokio [`Engine`] through [`Agent::ask`] with a scripted
//! mock model (no network) and asserts:
//! - a fresh ask persists a root trace; a follow-up ask persists a NEW child
//!   with `depends_on` set and never mutates the parent;
//! - forking is just asking the same parent twice → sibling traces;
//! - the store's lineage matches the three-way fork root → t1 → {t2a, t2b};
//! - every persisted trace replays bit-for-bit (`hugr_replay::verify`), so the
//!   resumed fold is deterministic.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{
    Agent, AnswerHook, Ask, AskHook, Pricing, ResponseContract, STATUS_ERROR, STATUS_SUCCESS,
    TraceId, TraceStore,
};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, Record, Usage};
use hugr_host::{Clock, ModelAdapter, ModelSink};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// A scripted model: pops the next queued reply per call. Text-only replies
/// (no tool calls) end the turn, which is all these fork tests need.
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
            .ok_or_else(|| anyhow::anyhow!("mock ran out of scripted replies"))?;
        sink.text(text.clone());
        Ok((ModelOutput::text(text), Usage::new(7, 3)))
    }
}

/// Deterministic host clock: a monotonic counter, so recorded traces are
/// reproducible and `verify()` is meaningful.
fn deterministic_clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

fn agent(store: TraceStore, replies: Vec<&'static str>) -> Agent {
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

fn priced_agent(store: TraceStore, replies: Vec<&'static str>) -> Agent {
    {
        let mut agent = Agent::new("test-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), MockModel::new(replies)));
        agent.system_prompt = Some("You answer tersely.".into());
        agent.clock = Some(deterministic_clock());
        agent.pricing = Pricing::new().with_tier("medium", 2.0, 5.0);
        agent
    }
}

/// Read a stored trace's raw bytes for a byte-for-byte unchanged assertion.
fn raw_bytes(store: &TraceStore, id: &TraceId) -> Vec<u8> {
    std::fs::read(store.path_of(id)).expect("trace file exists")
}

#[tokio::test]
async fn fresh_ask_persists_a_root_trace() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store.clone(), vec!["Paris."]);

    let answer = agent.ask(Ask::new("Capital of France?")).await.unwrap();

    assert_eq!(answer.status, STATUS_SUCCESS);
    assert_eq!(text_response(&answer.response), "Paris.");

    let head = store.head(&answer.trace_id).unwrap();
    assert_eq!(head.depends_on, None, "a fresh ask is a root trace");
    assert_eq!(head.agent_name, "test-agent");
    assert_eq!(head.question, "Capital of France?");
    assert_eq!(head.status, "success");

    // The new turn is billed: one model call, its usage folded in.
    assert_eq!(answer.metadata.model_calls, 1);
    assert_eq!(answer.metadata.tokens_in, 7);
    assert_eq!(answer.metadata.tokens_out, 3);

    // The persisted trace replays bit-for-bit.
    hugr_replay::verify(&store.get(&answer.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn typed_response_contract_retries_until_output_casts() {
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct TestResponse {
        response: String,
        related_documents: Vec<String>,
    }

    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let mut agent = agent(
        store,
        vec![
            "{\"answer\":\"Paris.\",\"related_documents\":[\"guide.md\"]}",
            "```json\n{\"response\":\"Paris.\",\"related_documents\":[\"guide.md\"]}\n```",
        ],
    );
    agent.response_contract = Some(ResponseContract::from_type::<TestResponse>("test_response"));

    let answer = agent.ask(Ask::new("capital?")).await.unwrap();
    assert_eq!(answer.status, STATUS_SUCCESS);
    assert_eq!(answer.response["response"], "Paris.");
    assert_eq!(answer.response["related_documents"], json!(["guide.md"]));
    assert_eq!(answer.metadata.model_calls, 2, "bad cast is retried once");
}

#[tokio::test]
async fn ask_hook_runs_before_the_turn() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let mut agent = agent(store, vec!["hooked"]);
    agent.ask_hooks.push(AskHook::new("prefix", |ask| {
        ask.question = format!("hooked: {}", ask.question);
    }));

    let answer = agent.ask(Ask::new("question")).await.unwrap();

    assert_eq!(answer.status, STATUS_SUCCESS);
    assert_eq!(answer.response["text"], "hooked");
    let head = agent.store().head(&answer.trace_id).unwrap();
    assert_eq!(head.question, "hooked: question");
}

#[tokio::test]
async fn answer_hook_runs_at_the_final_return_boundary() {
    #[derive(Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct ModelResponse {
        response: String,
        related_documents: Vec<String>,
    }

    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let mut agent = agent(
        store.clone(),
        vec![
            "{\"response\":\"Use advanced compute.\",\"related_documents\":[\"hub/advanced-compute-options.md\"]}",
        ],
    );
    agent.response_contract = Some(ResponseContract::from_type::<ModelResponse>(
        "model_response",
    ));
    agent
        .answer_hooks
        .push(AnswerHook::new("document_urls", |answer| {
            let related = answer
                .response
                .get_mut("related_documents")
                .and_then(Value::as_array_mut)
                .expect("related documents array");
            for document in related {
                let path = document.as_str().unwrap().to_string();
                let slug = path.strip_suffix(".md").unwrap_or(&path);
                let url = format!("https://huggingface.co/docs/{slug}");
                *document = json!({
                    "path": path,
                    "url": url,
                });
            }
        }));

    let answer = agent.ask(Ask::new("compute?")).await.unwrap();

    assert_eq!(answer.status, STATUS_SUCCESS);
    assert_eq!(
        answer.response["related_documents"],
        json!([{
            "path": "hub/advanced-compute-options.md",
            "url": "https://huggingface.co/docs/hub/advanced-compute-options",
        }])
    );

    let trace = store.get(&answer.trace_id).unwrap();
    let recorded_text = trace
        .log
        .iter()
        .rev()
        .find_map(|entry| match &entry.record {
            Record::ModelOutput { output, .. } => Some(output.text.as_str()),
            _ => None,
        });
    assert_eq!(
        recorded_text,
        Some(
            "{\"response\":\"Use advanced compute.\",\"related_documents\":[\"hub/advanced-compute-options.md\"]}"
        )
    );
    hugr_replay::verify(&trace).unwrap();
}

#[tokio::test]
async fn follow_up_writes_a_child_and_leaves_the_parent_untouched() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store.clone(), vec!["Paris.", "About 2.1 million."]);

    let root = agent.ask(Ask::new("Capital of France?")).await.unwrap();
    let parent_bytes = raw_bytes(&store, &root.trace_id);

    let child = agent
        .ask(Ask {
            trace_id: Some(root.trace_id.clone()),
            ..Ask::new("And its population?")
        })
        .await
        .unwrap();

    assert_ne!(child.trace_id, root.trace_id, "follow-up is a NEW trace");
    assert_eq!(text_response(&child.response), "About 2.1 million.");

    let child_head = store.head(&child.trace_id).unwrap();
    assert_eq!(child_head.depends_on, Some(root.trace_id.clone()));

    // Only the new turn is billed on the child, not the re-folded ancestry.
    assert_eq!(child.metadata.model_calls, 1);

    // The parent file is byte-for-byte unchanged.
    assert_eq!(
        parent_bytes,
        raw_bytes(&store, &root.trace_id),
        "resume must never mutate the parent"
    );

    hugr_replay::verify(&store.get(&child.trace_id).unwrap()).unwrap();
}

#[tokio::test]
async fn pricing_cost_is_folded_from_only_the_new_trace_slice() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = priced_agent(store.clone(), vec!["root", "child"]);

    let root = agent.ask(Ask::new("root question")).await.unwrap();
    let child = agent
        .ask(Ask {
            trace_id: Some(root.trace_id.clone()),
            ..Ask::new("child question")
        })
        .await
        .unwrap();

    // Mock usage is 7 input + 3 output tokens. At 2/5 USD per M tokens, the
    // microUSD cost is 7*2 + 3*5 = 29. A resumed ask reports only its new turn.
    for answer in [&root, &child] {
        assert_eq!(answer.metadata.cost_micro_usd, 29);
        assert_eq!(answer.metadata.tokens_in, 7);
        assert_eq!(answer.metadata.tokens_out, 3);
        assert_eq!(answer.metadata.model_calls, 1);
    }
}

#[tokio::test]
async fn three_way_fork_root_t1_t2a_t2b() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    // root → t1, then t1 forked into two siblings t2a / t2b.
    let agent = agent(
        store.clone(),
        vec!["root-answer", "t1-answer", "t2a-answer", "t2b-answer"],
    );

    let root = agent.ask(Ask::new("root question")).await.unwrap();
    let t1 = agent
        .ask(Ask {
            trace_id: Some(root.trace_id.clone()),
            ..Ask::new("t1 question")
        })
        .await
        .unwrap();

    // Fork: ask the SAME parent (t1) twice → two independent siblings.
    let t2a = agent
        .ask(Ask {
            trace_id: Some(t1.trace_id.clone()),
            ..Ask::new("what-if A")
        })
        .await
        .unwrap();
    let t1_bytes_after_first_fork = raw_bytes(&store, &t1.trace_id);
    let t2b = agent
        .ask(Ask {
            trace_id: Some(t1.trace_id.clone()),
            ..Ask::new("what-if B")
        })
        .await
        .unwrap();

    // Four distinct traces.
    let ids = [&root.trace_id, &t1.trace_id, &t2a.trace_id, &t2b.trace_id];
    for (i, a) in ids.iter().enumerate() {
        for b in ids.iter().skip(i + 1) {
            assert_ne!(a, b, "all four fork traces are distinct");
        }
    }

    // Lineage as recorded in headers alone (no event folding).
    assert_eq!(store.head(&root.trace_id).unwrap().depends_on, None);
    assert_eq!(
        store.head(&t1.trace_id).unwrap().depends_on,
        Some(root.trace_id.clone())
    );
    assert_eq!(
        store.head(&t2a.trace_id).unwrap().depends_on,
        Some(t1.trace_id.clone())
    );
    assert_eq!(
        store.head(&t2b.trace_id).unwrap().depends_on,
        Some(t1.trace_id.clone()),
        "both forks depend on the same parent"
    );

    // Forking the second sibling did not touch the first fork's parent bytes.
    assert_eq!(
        t1_bytes_after_first_fork,
        raw_bytes(&store, &t1.trace_id),
        "sibling forks never contend on the shared parent"
    );

    // The store lists exactly the four traces and every one verifies.
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 4);
    for head in listed {
        hugr_replay::verify(&store.get(&head.trace_id).unwrap()).unwrap();
    }

    // Each answer carries the right response from its own branch.
    assert_eq!(text_response(&t2a.response), "t2a-answer");
    assert_eq!(text_response(&t2b.response), "t2b-answer");
}

#[tokio::test]
async fn missing_parent_is_an_infrastructure_error() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store.clone(), vec!["unused"]);

    let err = agent
        .ask(Ask {
            trace_id: Some(TraceId::new("does-not-exist")),
            ..Ask::new("q")
        })
        .await
        .unwrap_err();

    assert!(err.missing_trace().is_some(), "unknown parent → AskError");
}

#[tokio::test]
async fn failed_resume_does_not_return_the_parent_answer() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let agent = agent(store.clone(), vec!["parent answer"]);
    let parent = agent.ask(Ask::new("first")).await.unwrap();

    let resumed = agent
        .ask(Ask {
            trace_id: Some(parent.trace_id),
            ..Ask::new("follow-up")
        })
        .await
        .unwrap();

    assert_eq!(resumed.status, STATUS_ERROR);
    assert!(resumed.response["error"].as_str().is_some());
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
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A unique temp dir under the system temp root. Uniqueness comes from a
/// process-global counter plus the pid — no clock, no RNG.
fn tempdir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let path = std::env::temp_dir().join(format!("hugr-agent-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}

fn text_response(response: &Value) -> &str {
    response["text"].as_str().unwrap()
}
