//! The `shell` capability: run a command, streaming stdout line-by-line.

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::capability::{Capability, ChunkSink};

/// Runs a shell command via `sh -c`, streaming stdout lines as chunks and
/// returning the exit code, stdout and stderr.
pub struct Shell;

#[async_trait]
impl Capability for Shell {
    fn name(&self) -> &str {
        "shell"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "shell",
            "Run a shell command via `sh -c` and capture its output.",
            json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string", "description": "The command line to run." }
                },
                "required": ["cmd"]
            }),
        )
    }

    async fn invoke(&self, args: Value, sink: &ChunkSink) -> Result<Value, Value> {
        let cmd = args
            .get("cmd")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `cmd`" }))?;

        let mut child = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| json!({ "error": format!("failed to spawn: {e}") }))?;

        // Stream stdout lines as they arrive (transport chunks for live UI).
        let mut stdout_buf = String::new();
        if let Some(stdout) = child.stdout.take() {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                sink.chunk(Value::String(format!("{line}\n")));
                stdout_buf.push_str(&line);
                stdout_buf.push('\n');
            }
        }

        let output = child
            .wait_with_output()
            .await
            .map_err(|e| json!({ "error": format!("failed to wait: {e}") }))?;
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();

        Ok(json!({
            "exit_code": output.status.code(),
            "stdout": stdout_buf,
            "stderr": stderr,
        }))
    }
}
