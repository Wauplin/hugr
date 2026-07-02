//! The `shell` capability: run a command, streaming stdout line-by-line.

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
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

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        // Consume stdout and stderr *concurrently*. Reading stdout to EOF
        // before touching stderr would deadlock: a child that fills the
        // kernel's ~64KB stderr pipe buffer blocks on write and never closes
        // stdout, so the stdout loop never finishes. `join!` interleaves both
        // readers on this task, draining each pipe as the child writes.
        let stdout_fut = async {
            // Stream stdout lines as they arrive (transport chunks for live UI).
            let mut stdout_buf = String::new();
            if let Some(stdout) = stdout_pipe {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    sink.chunk(Value::String(format!("{line}\n")));
                    stdout_buf.push_str(&line);
                    stdout_buf.push('\n');
                }
            }
            stdout_buf
        };
        let stderr_fut = async {
            let mut stderr_buf = Vec::new();
            if let Some(mut stderr) = stderr_pipe {
                // Best-effort: a read error just yields what arrived so far.
                let _ = stderr.read_to_end(&mut stderr_buf).await;
            }
            String::from_utf8_lossy(&stderr_buf).to_string()
        };
        let (stdout_buf, stderr) = tokio::join!(stdout_fut, stderr_fut);

        let status = child
            .wait()
            .await
            .map_err(|e| json!({ "error": format!("failed to wait: {e}") }))?;

        Ok(json!({
            "exit_code": status.code(),
            "stdout": stdout_buf,
            "stderr": stderr,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_core::{Event, OpId};
    use tokio::sync::mpsc;

    // Regression: a child that fills the ~64KB kernel stderr pipe buffer while
    // stdout is still open must not deadlock the capability (the old code read
    // stdout to EOF before ever draining stderr). Bounded by a timeout so a
    // regression fails fast instead of hanging CI.
    #[tokio::test]
    async fn large_stderr_alongside_stdout_does_not_deadlock() {
        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        let sink = ChunkSink::new(OpId(0), tx);

        // ~200KB of stderr (well past the pipe buffer), written before stdout
        // closes, plus a stdout line to keep the stdout pipe open meanwhile.
        let cmd = "yes e | head -c 200000 1>&2; echo out-line";
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Shell.invoke(json!({ "cmd": cmd }), &sink),
        )
        .await
        .expect("shell capability deadlocked on a full stderr pipe")
        .expect("command should succeed");

        assert_eq!(result["exit_code"], json!(0));
        assert_eq!(result["stdout"], json!("out-line\n"));
        assert_eq!(result["stderr"].as_str().unwrap().len(), 200_000);
    }
}
