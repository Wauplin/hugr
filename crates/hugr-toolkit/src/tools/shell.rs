use std::collections::BTreeSet;

use anyhow::{Context, Result};
use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde_json::json;

const DEFAULT_MAX_OUTPUT_BYTES: usize = 1_000_000;

/// An operator-configured full shell or direct allowlisted command runner.
pub struct Shell {
    full_access: bool,
    shell: String,
    allow_commands: BTreeSet<String>,
    cwd: Option<String>,
    max_output_bytes: usize,
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
        let output = process
            .output()
            .await
            .with_context(|| format!("executing `{command}`"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let truncate = |text: &str| -> (String, bool) {
            if text.len() <= self.max_output_bytes {
                return (text.to_string(), false);
            }
            let mut end = self.max_output_bytes;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            (text[..end].to_string(), true)
        };
        let (stdout, stdout_truncated) = truncate(&stdout);
        let (stderr, stderr_truncated) = truncate(&stderr);
        Ok(
            json!({"success": output.status.success(), "exit_code": output.status.code(), "stdout": stdout, "stderr": stderr, "truncated": stdout_truncated || stderr_truncated}),
        )
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
}
