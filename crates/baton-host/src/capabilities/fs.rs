//! The `fs_read` and `fs_write` capabilities.

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use serde_json::json;

use crate::capability::{Capability, ChunkSink};

/// Reads a file's contents. Read-only, so it does not require permission.
pub struct FsRead;

#[async_trait]
impl Capability for FsRead {
    fn name(&self) -> &str {
        "fs_read"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_read",
            "Read the contents of a UTF-8 text file.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read." }
                },
                "required": ["path"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `path`" }))?;

        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(json!({ "path": path, "content": content })),
            Err(e) => Err(json!({ "error": format!("failed to read {path}: {e}") })),
        }
    }
}

/// Writes contents to a file (creating or truncating it). Mutating, so it
/// requires permission by default.
pub struct FsWrite;

#[async_trait]
impl Capability for FsWrite {
    fn name(&self) -> &str {
        "fs_write"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "fs_write",
            "Write text to a file, creating or overwriting it.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to write." },
                    "content": { "type": "string", "description": "The full contents to write." }
                },
                "required": ["path", "content"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `path`" }))?;
        let content = args
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `content`" }))?;

        match tokio::fs::write(path, content).await {
            Ok(()) => Ok(json!({ "path": path, "bytes_written": content.len() })),
            Err(e) => Err(json!({ "error": format!("failed to write {path}: {e}") })),
        }
    }
}
