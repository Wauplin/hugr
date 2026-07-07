//! Resource groups & grants (ARCHITECTURE §18.5, ROADMAP T3.7).
//!
//! A tool bound to a resource group is registered only when an ask carries a
//! grant of sufficient access over that group. We observe the *effective tool
//! set* through the `ModelRequest.tools` the mock model receives, and assert:
//! - a `Read` grant registers the read-class tool but not the write-class one;
//! - a `ReadWrite` grant registers both;
//! - no grant registers neither (an ungranted bound tool is absent from the
//!   advertised schemas);
//! - a resume with no new grants re-derives the identical registration from the
//!   parent trace alone.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hugr_agent::{
    Access, Agent, Ask, GroupBinding, ResourceGrant, ResourceGroup, ResourceRef, TraceStore,
};
use hugr_core::{ModelOutput, ModelRequest, ModelSelector, Usage};
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink, Clock, ModelAdapter, ModelSink};
use serde_json::json;

/// Records the advertised tool names from the last model request.
#[derive(Clone, Default)]
struct ToolSpy(Arc<Mutex<Vec<String>>>);

struct MockModel {
    spy: ToolSpy,
}

#[async_trait]
impl ModelAdapter for MockModel {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        *self.spy.0.lock().unwrap() = request.tools.iter().map(|t| t.name.clone()).collect();
        sink.text("done".to_string());
        Ok((ModelOutput::text("done"), Usage::new(1, 1)))
    }
}

/// A do-nothing capability with a given name (a stand-in for a group-bound
/// read/write tool).
struct NoopTool(&'static str);

#[async_trait]
impl Capability for NoopTool {
    fn name(&self) -> &str {
        self.0
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(self.0, "noop", json!({ "type": "object" }))
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, _args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        Ok(json!({}))
    }
}

fn clock() -> Clock {
    let counter = Arc::new(AtomicU64::new(1));
    Arc::new(move || counter.fetch_add(1, Ordering::SeqCst))
}

/// An agent with a read-class tool (`reader`, needs Read) and a write-class
/// tool (`writer`, needs ReadWrite), both bound to the `data` group.
fn agent(store: TraceStore, spy: ToolSpy) -> Agent {
    let reader: hugr_agent::GroupCapabilityFactory = Arc::new(|_res: &[ResourceRef]| {
        Ok(vec![Arc::new(NoopTool("reader")) as Arc<dyn Capability>])
    });
    let writer: hugr_agent::GroupCapabilityFactory = Arc::new(|_res: &[ResourceRef]| {
        Ok(vec![Arc::new(NoopTool("writer")) as Arc<dyn Capability>])
    });
    {
        let mut agent = Agent::new("test-agent", "0.1.0", store);
        agent
            .models
            .push((ModelSelector::named("medium"), Arc::new(MockModel { spy })));
        agent.system_prompt = Some("answer".into());
        agent.clock = Some(clock());
        agent
            .group_bindings
            .push(GroupBinding::new("data", "reader", Access::Read, reader));
        agent.group_bindings.push(GroupBinding::new(
            "data",
            "writer",
            Access::ReadWrite,
            writer,
        ));
        agent
    }
}

fn data_group() -> ResourceGroup {
    ResourceGroup::new("data", vec![ResourceRef::FsRoot { path: ".".into() }])
}

fn advertised(spy: &ToolSpy) -> Vec<String> {
    spy.0.lock().unwrap().clone()
}

#[tokio::test]
async fn grants_attenuate_the_effective_tool_set() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let spy = ToolSpy::default();
    let agent = agent(store.clone(), spy.clone());

    // No grant → neither group-bound tool is registered.
    agent
        .ask(Ask::new("q").with_groups(vec![data_group()]))
        .await
        .unwrap();
    let tools = advertised(&spy);
    assert!(!tools.contains(&"reader".to_string()), "{tools:?}");
    assert!(!tools.contains(&"writer".to_string()), "{tools:?}");
    // Scratch tools are always present, proving the model did run.
    assert!(tools.contains(&"scratch_read".to_string()), "{tools:?}");

    // Read grant → reader only.
    agent
        .ask(
            Ask::new("q")
                .with_groups(vec![data_group()])
                .with_grants(vec![ResourceGrant::new("data", Access::Read)]),
        )
        .await
        .unwrap();
    let tools = advertised(&spy);
    assert!(tools.contains(&"reader".to_string()), "{tools:?}");
    assert!(!tools.contains(&"writer".to_string()), "{tools:?}");

    // ReadWrite grant → both.
    agent
        .ask(
            Ask::new("q")
                .with_groups(vec![data_group()])
                .with_grants(vec![ResourceGrant::new("data", Access::ReadWrite)]),
        )
        .await
        .unwrap();
    let tools = advertised(&spy);
    assert!(tools.contains(&"reader".to_string()), "{tools:?}");
    assert!(tools.contains(&"writer".to_string()), "{tools:?}");
}

#[tokio::test]
async fn resume_re_derives_registration_from_the_trace_alone() {
    let dir = tempdir();
    let store = TraceStore::new(dir.path());
    let spy = ToolSpy::default();
    let agent = agent(store.clone(), spy.clone());

    // Root ask with a ReadWrite grant.
    let root = agent
        .ask(
            Ask::new("root")
                .with_groups(vec![data_group()])
                .with_grants(vec![ResourceGrant::new("data", Access::ReadWrite)]),
        )
        .await
        .unwrap();
    assert!(advertised(&spy).contains(&"writer".to_string()));

    // The grants are recorded in the trace header.
    let trace = store.get(&root.trace_id).unwrap();
    assert!(trace.meta.grants.is_some(), "grants recorded in the trace");

    // Resume with NO new groups/grants → re-derives the parent's ReadWrite
    // registration from the trace alone (both tools advertised again).
    agent
        .ask(Ask::new("follow up").with_trace_id(root.trace_id.clone()))
        .await
        .unwrap();
    let tools = advertised(&spy);
    assert!(tools.contains(&"reader".to_string()), "{tools:?}");
    assert!(tools.contains(&"writer".to_string()), "{tools:?}");

    // A fork that changes the grant to Read is a *new recorded fact* on the
    // fork's trace (§18.5): its recorded grants differ from the parent's, and
    // the parent is untouched. (Execution-time registration follows the new
    // grant; the model-advertised schema set on a resumed turn follows the
    // resumed policy, matching the engine's resume semantics.)
    let fork = agent
        .ask(
            Ask::new("what-if read only")
                .with_trace_id(root.trace_id.clone())
                .with_groups(vec![data_group()])
                .with_grants(vec![ResourceGrant::new("data", Access::Read)]),
        )
        .await
        .unwrap();
    let fork_grants = store.get(&fork.trace_id).unwrap().meta.grants.unwrap();
    assert_eq!(fork_grants["grants"][0]["access"], json!("read"));
    // The parent's recorded grant is still ReadWrite — never mutated by the fork.
    let parent_grants = store.get(&root.trace_id).unwrap().meta.grants.unwrap();
    assert_eq!(parent_grants["grants"][0]["access"], json!("read_write"));
}

// --- tiny tempdir helper -------------------------------------------------

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
    let path = std::env::temp_dir().join(format!("hugr-agent-groups-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    TempDir { path }
}
