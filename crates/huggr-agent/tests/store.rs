//! Trace-store tests: persist a trace, list lineage via `depends_on`, read
//! headers without loading events, and keep pre-store `huggr-replay` traces
//! loading unchanged.

use huggr_agent::{StoreError, TraceHeader, TraceId, TraceStore};
use huggr_replay::Trace;
use huggr_replay::test_support::TempDir;

/// Pin a trace file's mtime so LRU/age ordering is deterministic in tests.
fn empty_trace(created_at: u64) -> Trace {
    Trace::new(Vec::new(), Vec::new(), Some(created_at))
}

fn header(question: &str) -> TraceHeader {
    TraceHeader::new("docs", "0.1.0", question, "success")
}

/// A run persists a trace: `put` stamps the header + generated id, `get`
/// returns the full trace (one file read, then a pure parse), and the stored
/// trace is immutable — re-putting the same content lands as a **new** sibling
/// id, never an overwrite.
#[test]
fn put_persists_and_get_reloads() {
    let dir = TempDir::new("agent-store-put");
    let store = TraceStore::new(dir.path());

    let id = store.put(empty_trace(7), header("what is huggr?")).unwrap();
    let loaded = store.get(&id).unwrap();
    assert_eq!(loaded.meta.trace_id.as_deref(), Some(id.as_str()));
    assert_eq!(loaded.meta.depends_on, None);
    assert_eq!(loaded.meta.agent_name.as_deref(), Some("docs"));
    assert_eq!(loaded.meta.agent_version.as_deref(), Some("0.1.0"));
    assert_eq!(loaded.meta.question.as_deref(), Some("what is huggr?"));
    assert_eq!(loaded.meta.status.as_deref(), Some("success"));
    assert_eq!(loaded.meta.created_at, Some(7));
    assert!(loaded.events.is_empty() && loaded.log.is_empty());

    // Immutability: identical content re-put gets a fresh (suffixed) id and
    // the original file is untouched.
    let before = std::fs::read(store.path_of(&id)).unwrap();
    let id2 = store.put(empty_trace(7), header("what is huggr?")).unwrap();
    assert_ne!(id, id2);
    assert_eq!(id2.as_str(), format!("{id}-1"));
    assert_eq!(std::fs::read(store.path_of(&id)).unwrap(), before);
}

/// Ids are content-derived (no RNG, no clock): the same trace bytes into two
/// fresh stores propose the same id; different content gets a different id.
#[test]
fn trace_ids_are_deterministic_from_content() {
    let dir_a = TempDir::new("agent-store-det-a");
    let dir_b = TempDir::new("agent-store-det-b");
    let id_a = TraceStore::new(dir_a.path())
        .put(empty_trace(7), header("q"))
        .unwrap();
    let id_b = TraceStore::new(dir_b.path())
        .put(empty_trace(7), header("q"))
        .unwrap();
    assert_eq!(id_a, id_b);

    let id_c = TraceStore::new(dir_a.path())
        .put(empty_trace(8), header("q"))
        .unwrap();
    assert_ne!(id_a, id_c);
}

/// `list()` shows lineage: a root → t1 → {t2a, t2b} fork is fully visible from
/// the headers' `depends_on` pointers alone, in deterministic (id-sorted) order.
#[test]
fn list_shows_lineage_with_depends_on() {
    let dir = TempDir::new("agent-store-lineage");
    let store = TraceStore::new(dir.path());

    let root = store.put(empty_trace(1), header("root")).unwrap();
    let t1 = store
        .put(
            empty_trace(2),
            header("follow-up").with_depends_on(root.clone()),
        )
        .unwrap();
    let t2a = store
        .put(
            empty_trace(3),
            header("branch a").with_depends_on(t1.clone()),
        )
        .unwrap();
    let t2b = store
        .put(
            empty_trace(4),
            header("branch b").with_depends_on(t1.clone()),
        )
        .unwrap();

    let heads = store.list().unwrap();
    assert_eq!(heads.len(), 4);
    let parent_of = |id: &TraceId| -> Option<TraceId> {
        heads
            .iter()
            .find(|h| &h.trace_id == id)
            .unwrap()
            .depends_on
            .clone()
    };
    assert_eq!(parent_of(&root), None);
    assert_eq!(parent_of(&t1), Some(root.clone()));
    assert_eq!(parent_of(&t2a), Some(t1.clone()));
    assert_eq!(parent_of(&t2b), Some(t1.clone()));
    // Deterministic listing order regardless of directory-entry order.
    let mut sorted: Vec<_> = heads.iter().map(|h| h.trace_id.clone()).collect();
    let listed = sorted.clone();
    sorted.sort();
    assert_eq!(listed, sorted);
}

/// `head()` returns the full header without loading events, even when the
/// stored trace has a fat body; a missing id errors with `NotFound`.
#[test]
fn head_reads_metadata_without_full_load() {
    let dir = TempDir::new("agent-store-head");
    let store = TraceStore::new(dir.path());

    let parent = TraceId::new("p0");
    let id = store
        .put(
            empty_trace(42),
            header("heads up").with_depends_on(parent.clone()),
        )
        .unwrap();

    // Corrupt the trace *body* on disk while keeping `meta` intact: head()
    // must still succeed because it only parses the header, proving the
    // events are never deserialized.
    let path = store.path_of(&id);
    let json = std::fs::read_to_string(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
    value["events"] = serde_json::json!(["not", "an", "event"]);
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();

    let head = store.head(&id).unwrap();
    assert_eq!(head.trace_id, id);
    assert_eq!(head.depends_on, Some(parent));
    assert_eq!(head.agent_name, "docs");
    assert_eq!(head.agent_version, "0.1.0");
    assert_eq!(head.created_at, Some(42));
    assert_eq!(head.question, "heads up");
    assert_eq!(head.status, "success");
    // …while a full get() on the corrupted body now fails, confirming head()
    // took the metadata-only path.
    assert!(store.get(&id).is_err());

    match store.head(&TraceId::new("nope")) {
        Err(StoreError::NotFound { id }) => assert_eq!(id.as_str(), "nope"),
        other => panic!("expected NotFound, got {other:?}"),
    }
}

#[test]
fn pre_store_traces_load_and_stay_byte_stable() {
    // Old JSON without any of the new meta fields keeps loading.
    let old_json = r#"{
        "meta": { "codename": "huggr-trace", "format_version": 1, "created_at": 5 },
        "events": [],
        "log": [],
        "blobs": { "refs": [] }
    }"#;
    let trace = Trace::from_json(old_json.as_bytes()).unwrap();
    assert_eq!(trace.meta.trace_id, None);
    assert_eq!(trace.meta.depends_on, None);
    assert_eq!(trace.meta.agent_name, None);

    // A trace never touched by a store serializes with no new keys at all.
    let bytes = empty_trace(5).to_json().unwrap();
    let text = String::from_utf8(bytes).unwrap();
    for key in [
        "trace_id",
        "depends_on",
        "agent_name",
        "agent_version",
        "question",
        "status",
    ] {
        assert!(!text.contains(key), "unexpected key `{key}` in {text}");
    }
    // And round-trips to itself.
    assert_eq!(Trace::from_json(text.as_bytes()).unwrap(), empty_trace(5));
}
