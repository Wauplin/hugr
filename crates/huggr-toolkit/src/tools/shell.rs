use std::collections::BTreeSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;
use tokio::io::AsyncReadExt;

const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;
const DEFAULT_TIMEOUT_S: u64 = 300;

/// An operator-configured full shell or direct allowlisted command runner.
pub struct Shell {
    full_access: bool,
    shell: String,
    allow_commands: BTreeSet<String>,
    cwd: Option<String>,
    max_output_bytes: usize,
    timeout: std::time::Duration,
}

impl Shell {
    /// Build a shell capability from one `[tools.shell]` grant.
    pub fn from_config(config: &Value) -> Result<Self> {
        let full_access = config
            .get("full_access")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let allow_commands: BTreeSet<String> = config
            .get("allow_commands")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        anyhow::ensure!(
            full_access || !allow_commands.is_empty(),
            "set `full_access = true` or a non-empty `allow_commands` array"
        );
        anyhow::ensure!(
            !full_access || allow_commands.is_empty(),
            "`full_access` and `allow_commands` are mutually exclusive"
        );
        Ok(Self {
            full_access,
            shell: config
                .get("shell")
                .and_then(Value::as_str)
                .unwrap_or("/bin/sh")
                .to_string(),
            allow_commands,
            cwd: config
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_string),
            max_output_bytes: config
                .get("max_output_bytes")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_OUTPUT_BYTES),
            timeout: std::time::Duration::from_secs(
                config
                    .get("timeout_s")
                    .and_then(Value::as_u64)
                    .filter(|s| *s > 0)
                    .unwrap_or(DEFAULT_TIMEOUT_S),
            ),
        })
    }

    async fn run(&self, args: Value) -> Result<Value> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .context("shell requires string `command`")?;
        let mut process = if self.full_access {
            let mut process = tokio::process::Command::new(&self.shell);
            process.arg("-lc").arg(command);
            process
        } else {
            anyhow::ensure!(
                self.allow_commands.contains(command),
                "command `{command}` is not allowlisted"
            );
            anyhow::ensure!(
                !command.chars().any(char::is_whitespace),
                "restricted command must be one executable name without whitespace"
            );
            let mut process = tokio::process::Command::new(command);
            if let Some(argv) = args.get("args").and_then(Value::as_array) {
                for arg in argv {
                    process.arg(
                        arg.as_str()
                            .context("every restricted command argument must be a string")?,
                    );
                }
            }
            process
        };
        if let Some(cwd) = &self.cwd {
            process.current_dir(cwd);
        }
        process
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = process
            .spawn()
            .with_context(|| format!("executing `{command}`"))?;
        let stdout = child.stdout.take().expect("stdout was piped");
        let stderr = child.stderr.take().expect("stderr was piped");
        let run = async {
            let (status, stdout, stderr) = tokio::join!(
                child.wait(),
                read_capped(stdout, self.max_output_bytes),
                read_capped(stderr, self.max_output_bytes)
            );
            anyhow::Ok((status?, stdout?, stderr?))
        };
        // On timeout the future is dropped, which drops the child; kill_on_drop
        // terminates the process so a hung command cannot outlive the ask.
        let (status, stdout, stderr) =
            tokio::time::timeout(self.timeout, run)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "command `{command}` timed out after {}s (process killed)",
                        self.timeout.as_secs()
                    )
                })??;
        let (stdout, stdout_truncated) = stdout;
        let (stderr, stderr_truncated) = stderr;
        Ok(
            json!({"success": status.success(), "exit_code": status.code(), "stdout": String::from_utf8_lossy(&stdout), "stderr": String::from_utf8_lossy(&stderr), "truncated": stdout_truncated || stderr_truncated}),
        )
    }
}

/// Capture at most `cap` bytes and keep draining so the child never blocks on
/// a full pipe; returns the captured prefix and whether output was truncated.
async fn read_capped(
    mut reader: impl tokio::io::AsyncRead + Unpin,
    cap: usize,
) -> std::io::Result<(Vec<u8>, bool)> {
    let mut captured = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok((captured, truncated));
        }
        let remaining = cap.saturating_sub(captured.len());
        if count > remaining {
            truncated = true;
        }
        captured.extend_from_slice(&chunk[..count.min(remaining)]);
    }
}

#[async_trait]
impl Capability for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> ToolSchema {
        let description = if self.full_access {
            "Run a command through the operator-granted full shell."
        } else {
            "Run one operator-allowlisted executable directly. Shell syntax, chaining, pipes, redirection, expansion, and globbing are unavailable."
        };
        ToolSchema::new(
            "shell",
            description,
            json!({"type":"object","properties":{"command":{"type":"string"},"args":{"type":"array","items":{"type":"string"},"description":"Arguments for restricted mode; ignored in full mode."}},"required":["command"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        self.run(args)
            .await
            .map_err(|e| json!({"error": e.to_string()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn restricted_mode_never_parses_shell_syntax() {
        let tool = Shell::from_config(&json!({"allow_commands":["printf"]})).unwrap();
        assert!(
            tool.run(json!({"command":"printf && false","args":[]}))
                .await
                .unwrap_err()
                .to_string()
                .contains("not allowlisted")
        );
        let out = tool
            .run(json!({"command":"printf","args":["%s", "a&&b"]}))
            .await
            .unwrap();
        assert_eq!(out["stdout"], "a&&b");
    }

    #[tokio::test]
    async fn a_hung_command_is_killed_at_the_timeout() {
        let tool = Shell::from_config(&json!({"allow_commands":["sleep"],"timeout_s":1})).unwrap();
        let start = std::time::Instant::now();
        let err = tool
            .run(json!({"command":"sleep","args":["30"]}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn output_is_capped_without_blocking_the_child() {
        let tool = Shell::from_config(
            &json!({"allow_commands":["head"],"max_output_bytes":1024,"timeout_s":30}),
        )
        .unwrap();
        // 4 MiB of zeros through the pipe; capture must cap at 1 KiB and the
        // child must still run to completion (the reader keeps draining).
        let out = tool
            .run(json!({"command":"head","args":["-c","4194304","/dev/zero"]}))
            .await
            .unwrap();
        assert_eq!(out["truncated"], true);
        assert_eq!(out["success"], true);
        assert!(out["stdout"].as_str().unwrap().len() <= 1024);
    }
}
