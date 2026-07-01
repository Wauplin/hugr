//! Patch preview/apply/revert capability (ROADMAP_2 D3).
//!
//! This keeps patch mechanics in the host. Failures are semantic tool results
//! the model can react to; the brain just routes opaque JSON.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::capability::{Capability, ChunkSink};

pub struct PatchApply;

#[async_trait]
impl Capability for PatchApply {
    fn name(&self) -> &str {
        "patch_apply"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "patch_apply",
            "Preview, apply, or revert a unified diff using git apply. Patch failures are returned as conflicts.",
            json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "Unified diff to process." },
                    "mode": {
                        "type": "string",
                        "enum": ["preview", "apply", "revert"],
                        "description": "preview checks whether the patch applies; apply mutates; revert applies the diff in reverse."
                    },
                    "root": { "type": "string", "description": "Working directory. Defaults to current directory." }
                },
                "required": ["patch", "mode"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let patch = args
            .get("patch")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `patch`" }))?;
        let mode = args
            .get("mode")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `mode`" }))?;
        let root = args
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));

        let mut cmd = Command::new("git");
        cmd.current_dir(&root)
            .arg("apply")
            .arg("--whitespace=nowarn");
        match mode {
            "preview" => {
                cmd.arg("--check");
            }
            "apply" => {}
            "revert" => {
                cmd.arg("--reverse");
            }
            other => return Err(json!({ "error": format!("unknown patch mode `{other}`") })),
        }
        let output = run_git_apply(cmd, patch).await?;
        if output.status.success() {
            Ok(json!({
                "mode": mode,
                "status": match mode {
                    "preview" => "preview_ok",
                    "apply" => "applied",
                    "revert" => "reverted",
                    _ => unreachable!(),
                },
                "root": root.display().to_string(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }))
        } else {
            Err(json!({
                "error": "conflict",
                "mode": mode,
                "root": root.display().to_string(),
                "exit_code": output.status.code(),
                "stdout": String::from_utf8_lossy(&output.stdout),
                "stderr": String::from_utf8_lossy(&output.stderr),
            }))
        }
    }
}

async fn run_git_apply(mut cmd: Command, patch: &str) -> Result<std::process::Output, Value> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| json!({ "error": format!("failed to spawn git apply: {e}") }))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| json!({ "error": "failed to open git apply stdin" }))?;
    stdin
        .write_all(patch.as_bytes())
        .await
        .map_err(|e| json!({ "error": format!("failed to write patch to git apply: {e}") }))?;
    drop(stdin);
    child
        .wait_with_output()
        .await
        .map_err(|e| json!({ "error": format!("failed to wait for git apply: {e}") }))
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use hugr_core::{Event, OpId};
    use tokio::sync::mpsc;

    use super::*;

    #[tokio::test]
    async fn patch_previews_applies_reverts_and_conflicts() {
        let root = std::env::temp_dir().join(format!(
            "hugr_patch_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("demo.txt");
        std::fs::write(&file, "old\n").unwrap();
        let patch = "\
diff --git a/demo.txt b/demo.txt
--- a/demo.txt
+++ b/demo.txt
@@ -1 +1 @@
-old
+new
";
        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        let sink = ChunkSink::new(OpId(0), tx);

        let preview = PatchApply
            .invoke(
                json!({ "root": root, "mode": "preview", "patch": patch }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(preview["status"], json!("preview_ok"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");

        let apply = PatchApply
            .invoke(
                json!({ "root": file.parent().unwrap(), "mode": "apply", "patch": patch }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(apply["status"], json!("applied"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "new\n");

        let conflict = PatchApply
            .invoke(
                json!({ "root": file.parent().unwrap(), "mode": "apply", "patch": patch }),
                &sink,
            )
            .await
            .expect_err("already-applied patch should conflict");
        assert_eq!(conflict["error"], json!("conflict"));

        let revert = PatchApply
            .invoke(
                json!({ "root": file.parent().unwrap(), "mode": "revert", "patch": patch }),
                &sink,
            )
            .await
            .unwrap();
        assert_eq!(revert["status"], json!("reverted"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "old\n");
    }
}
