//! The `blob` capability: a content-addressed store for large payloads.
//!
//! Large tool outputs / inputs do not belong inline in the durable log or in
//! every context projection (ARCHITECTURE §3.3). This capability lets the model
//! (or another capability) offload a big payload to a content-addressed store
//! and keep only a small hash reference, then rehydrate it on demand.
//!
//! It is an **ordinary [`Capability`]** — there are no privileged built-ins
//! (DESIGN §5.3): the host registers it exactly like `shell`/`fs`/`http`, and a
//! host that cannot provide a disk store (a browser, say) simply registers a
//! different store or none at all. The args/results are kept **opaque `Value`**
//! per the narrow-waist rule (ARCHITECTURE §2.4): the brain stores and forwards
//! them, it never interprets them.
//!
//! The disk-backed [`BlobStore`] itself lives in `baton-replay` (the host-side
//! persistence crate) so it shares the exact [`BlobRef`](baton_replay::BlobRef)
//! shape a trace's `BlobManifest` carries — a payload offloaded here can be
//! referenced by digest in a trace and rehydrated on load.

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use baton_replay::BlobStore;
use serde_json::json;

use crate::capability::{Capability, ChunkSink};

/// A content-addressed blob store exposed as a capability. Two operations,
/// selected by the opaque `op` argument:
///
/// - `put`  — `{ "op": "put", "content": "<text>", "media": "<type>"? }` →
///   `{ "hash", "len", "media" }` (a [`BlobRef`](baton_replay::BlobRef)).
/// - `get`  — `{ "op": "get", "hash": "sha256:..." }` → `{ "hash", "content" }`.
///
/// Storing identical content twice dedupes to the same hash.
pub struct Blob {
    store: BlobStore,
}

impl Blob {
    /// A blob capability backed by a disk store rooted at `root`.
    pub fn new(root: impl Into<std::path::PathBuf>) -> Self {
        Self {
            store: BlobStore::new(root),
        }
    }

    /// A blob capability wrapping an already-constructed [`BlobStore`] (so the
    /// host can share one store between the capability and trace persistence).
    pub fn with_store(store: BlobStore) -> Self {
        Self { store }
    }

    /// The underlying store (e.g. for the recorder to build a `BlobManifest`).
    pub fn store(&self) -> &BlobStore {
        &self.store
    }
}

#[async_trait]
impl Capability for Blob {
    fn name(&self) -> &str {
        "blob"
    }

    // Content-addressed storage of a payload is not a mutating side effect on
    // the user's environment (it is keyed by content and idempotent), so like
    // `fs_read` it does not gate on a permission round-trip.
    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "blob",
            "Content-addressed store for large payloads. `put` stores text and \
             returns its hash; `get` rehydrates the text for a hash.",
            json!({
                "type": "object",
                "properties": {
                    "op": {
                        "type": "string",
                        "enum": ["put", "get"],
                        "description": "Whether to store (`put`) or fetch (`get`) a blob."
                    },
                    "content": {
                        "type": "string",
                        "description": "The text payload to store (required for `put`)."
                    },
                    "media": {
                        "type": "string",
                        "description": "Media type for a `put` (default `text/plain`)."
                    },
                    "hash": {
                        "type": "string",
                        "description": "The content hash to fetch (required for `get`)."
                    }
                },
                "required": ["op"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let op = args.get("op").and_then(Value::as_str).ok_or_else(
            || json!({ "error": "missing string argument `op` (\"put\" or \"get\")" }),
        )?;

        match op {
            "put" => {
                let content = args
                    .get("content")
                    .and_then(Value::as_str)
                    .ok_or_else(|| json!({ "error": "missing string argument `content`" }))?;
                let media = args
                    .get("media")
                    .and_then(Value::as_str)
                    .unwrap_or("text/plain");

                let blob = self
                    .store
                    .put(content.as_bytes(), media)
                    .map_err(|e| json!({ "error": format!("blob put failed: {e}") }))?;

                Ok(json!({ "hash": blob.hash, "len": blob.len, "media": blob.media }))
            }
            "get" => {
                let hash = args
                    .get("hash")
                    .and_then(Value::as_str)
                    .ok_or_else(|| json!({ "error": "missing string argument `hash`" }))?;

                let bytes = self
                    .store
                    .get(hash)
                    .map_err(|e| json!({ "error": format!("blob get failed: {e}") }))?;
                let content = String::from_utf8(bytes)
                    .map_err(|e| json!({ "error": format!("blob is not valid UTF-8: {e}") }))?;

                Ok(json!({ "hash": hash, "content": content }))
            }
            other => Err(
                json!({ "error": format!("unknown op `{other}` (expected \"put\" or \"get\")") }),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ChunkSink;
    use baton_core::OpId;

    fn temp_root() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "baton-blobcap-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sink() -> ChunkSink {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ChunkSink::new(OpId(1), tx)
    }

    #[tokio::test]
    async fn put_then_get_roundtrips_a_large_payload() {
        let root = temp_root();
        let cap = Blob::new(&root);

        let big = "x".repeat(200_000);
        let put = cap
            .invoke(json!({ "op": "put", "content": big }), &sink())
            .await
            .unwrap();
        let hash = put.get("hash").and_then(Value::as_str).unwrap().to_string();
        assert_eq!(
            put.get("len").and_then(Value::as_u64),
            Some(big.len() as u64)
        );

        let got = cap
            .invoke(json!({ "op": "get", "hash": hash }), &sink())
            .await
            .unwrap();
        assert_eq!(
            got.get("content").and_then(Value::as_str),
            Some(big.as_str())
        );

        // The capability and the trace manifest share the same BlobRef shape, so
        // the stored payload is reachable from a trace by digest.
        assert!(
            cap.store()
                .contains(put.get("hash").and_then(Value::as_str).unwrap())
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn same_content_yields_same_hash() {
        let root = temp_root();
        let cap = Blob::new(&root);
        let a = cap
            .invoke(json!({ "op": "put", "content": "same" }), &sink())
            .await
            .unwrap();
        let b = cap
            .invoke(json!({ "op": "put", "content": "same" }), &sink())
            .await
            .unwrap();
        assert_eq!(
            a.get("hash").and_then(Value::as_str),
            b.get("hash").and_then(Value::as_str)
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn get_missing_is_a_semantic_error() {
        let cap = Blob::new(temp_root());
        let err = cap
            .invoke(json!({ "op": "get", "hash": "sha256:nope" }), &sink())
            .await
            .unwrap_err();
        assert!(err.get("error").is_some());
    }

    #[tokio::test]
    async fn unknown_op_is_a_semantic_error() {
        let cap = Blob::new(temp_root());
        let err = cap
            .invoke(json!({ "op": "delete" }), &sink())
            .await
            .unwrap_err();
        assert!(err.get("error").is_some());
    }
}
