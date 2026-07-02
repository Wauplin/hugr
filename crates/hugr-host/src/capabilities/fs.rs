//! The `fs_read` and `fs_write` capabilities.

use async_trait::async_trait;
use hugr_core::{ToolSchema, ToolVersioning, Value, VersionRef};
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
        .with_versioning(ToolVersioning::read("path"))
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `path`" }))?;

        match tokio::fs::read_to_string(path).await {
            Ok(content) => Ok(json!({
                "path": path,
                "content": content,
                "version": content_version(&content),
            })),
            Err(e) => Err(json!({ "error": format!("failed to read {path}: {e}") })),
        }
    }

    fn result_version(&self, result: &Value) -> Option<VersionRef> {
        version_ref_from_value(result)
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
                    "content": { "type": "string", "description": "The full contents to write." },
                    "expected_version": {
                        "type": "string",
                        "description": "Injected by Hugr when the file was previously read; do not invent."
                    }
                },
                "required": ["path", "content"]
            }),
        )
        .with_versioning(ToolVersioning::mutation("path", "expected_version"))
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
        if let Some(expected) = args.get("expected_version").and_then(Value::as_str) {
            match tokio::fs::read_to_string(path).await {
                Ok(current) => {
                    let current_version = content_version(&current);
                    if current_version != expected {
                        return Err(json!({
                            "error": "conflict",
                            "message": "file changed since it was read",
                            "path": path,
                            "current_version": current_version,
                            "current_content": current,
                        }));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if expected != missing_version() {
                        return Err(json!({
                            "error": "conflict",
                            "message": "file no longer exists or was not previously absent",
                            "path": path,
                            "current_version": missing_version(),
                            "current_content": null,
                        }));
                    }
                }
                Err(e) => return Err(json!({ "error": format!("failed to read {path}: {e}") })),
            }
        }

        match tokio::fs::write(path, content).await {
            Ok(()) => Ok(json!({
                "path": path,
                "bytes_written": content.len(),
                "version": content_version(content),
            })),
            Err(e) => Err(json!({ "error": format!("failed to write {path}: {e}") })),
        }
    }

    fn result_version(&self, result: &Value) -> Option<VersionRef> {
        version_ref_from_value(result)
    }

    fn conflict_version(&self, error: &Value) -> Option<VersionRef> {
        let object = error.get("path")?.as_str()?.to_string();
        let version = error.get("current_version")?.as_str()?.to_string();
        Some(VersionRef::new(object, version))
    }
}

fn version_ref_from_value(value: &Value) -> Option<VersionRef> {
    let object = value.get("path")?.as_str()?.to_string();
    let version = value.get("version")?.as_str()?.to_string();
    Some(VersionRef::new(object, version))
}

fn content_version(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn missing_version() -> &'static str {
    "missing"
}

#[cfg(test)]
mod tests {
    use hugr_core::{Event, OpId};
    use tokio::sync::mpsc;

    use super::*;
    use crate::test_support::TempDir;

    #[tokio::test]
    async fn fs_write_conflicts_on_stale_expected_version_without_overwriting() {
        let root = TempDir::new("fs-cas");
        let path = root.path().join("demo.txt");
        std::fs::write(&path, "first").unwrap();

        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        let sink = ChunkSink::new(OpId(0), tx);
        let read = FsRead
            .invoke(json!({ "path": path.display().to_string() }), &sink)
            .await
            .unwrap();
        let version = FsRead.result_version(&read).expect("read version");

        std::fs::write(&path, "changed elsewhere").unwrap();
        let error = FsWrite
            .invoke(
                json!({
                    "path": path.display().to_string(),
                    "content": "agent write",
                    "expected_version": version.version,
                }),
                &sink,
            )
            .await
            .expect_err("stale write should conflict");

        assert_eq!(error["error"], json!("conflict"));
        assert_eq!(
            FsWrite.conflict_version(&error).unwrap().object,
            path.display().to_string()
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed elsewhere");
    }
}
