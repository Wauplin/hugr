//! Cargo verification loop capability (ROADMAP_2 D6).
//!
//! The host owns process execution and streaming. The brain sees one ordinary
//! background capability result containing status plus a concise failure
//! summary the model can react to.

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::capability::{Capability, ChunkSink};

const DEFAULT_MAX_OUTPUT: usize = 64_000;

pub struct CargoVerify;

#[async_trait]
impl Capability for CargoVerify {
    fn name(&self) -> &str {
        "cargo_verify"
    }

    fn runs_in_background(&self) -> bool {
        true
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "cargo_verify",
            "Run cargo fmt, test, or clippy as a background verification op and summarize failures.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "enum": ["fmt", "test", "clippy"],
                        "description": "Verification command to run."
                    },
                    "root": { "type": "string", "description": "Workspace root. Defaults to current directory." },
                    "package": { "type": "string", "description": "Optional cargo package, passed as -p <package>." },
                    "test_filter": { "type": "string", "description": "Optional test filter for cargo test." },
                    "extra_args": { "type": "array", "items": { "type": "string" }, "description": "Additional cargo args." },
                    "changed_files": { "type": "array", "items": { "type": "string" }, "description": "Optional changed files used to suggest targeted tests." },
                    "retry_budget": { "type": "integer", "description": "Bounded repair-loop budget the model should respect." },
                    "max_output_bytes": { "type": "integer", "description": "Maximum captured stdout/stderr bytes." }
                },
                "required": ["command"]
            }),
        )
    }

    async fn invoke(&self, args: Value, sink: &ChunkSink) -> Result<Value, Value> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `command`" }))?;
        let root = args
            .get("root")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let max_output = args
            .get("max_output_bytes")
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(DEFAULT_MAX_OUTPUT);

        let mut cmd = Command::new("cargo");
        cmd.current_dir(&root);
        match command {
            "fmt" => {
                cmd.arg("fmt").arg("--all");
            }
            "test" => {
                cmd.arg("test");
                if let Some(package) = args.get("package").and_then(Value::as_str) {
                    cmd.arg("-p").arg(package);
                }
                if let Some(filter) = args.get("test_filter").and_then(Value::as_str) {
                    cmd.arg(filter);
                }
            }
            "clippy" => {
                cmd.arg("clippy").arg("--all-targets");
                if let Some(package) = args.get("package").and_then(Value::as_str) {
                    cmd.arg("-p").arg(package);
                }
            }
            other => {
                return Err(json!({ "error": format!("unknown cargo_verify command `{other}`") }));
            }
        }
        if let Some(extra) = args.get("extra_args").and_then(Value::as_array) {
            for arg in extra.iter().filter_map(Value::as_str) {
                cmd.arg(arg);
            }
        }

        let output = run_streaming(cmd, sink, max_output).await?;
        let combined = format!("{}\n{}", output.stdout, output.stderr);
        let summary = summarize_failures(&combined);
        let changed_files = args
            .get("changed_files")
            .and_then(Value::as_array)
            .map(|files| files.iter().filter_map(Value::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let targeted = targeted_test_hints(&changed_files);
        let retry_budget = args
            .get("retry_budget")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        Ok(json!({
            "command": command,
            "root": root.display().to_string(),
            "exit_code": output.exit_code,
            "success": output.exit_code == Some(0),
            "summary": summary,
            "targeted_tests": targeted,
            "retry_budget": retry_budget,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "truncated": output.truncated,
        }))
    }
}

struct CapturedOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    truncated: bool,
}

async fn run_streaming(
    mut cmd: Command,
    sink: &ChunkSink,
    max_output: usize,
) -> Result<CapturedOutput, Value> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| json!({ "error": format!("failed to spawn cargo: {e}") }))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let stdout_task = tokio::spawn(read_lines(stdout, sink.clone(), "stdout", max_output));
    let stderr_task = tokio::spawn(read_lines(stderr, sink.clone(), "stderr", max_output));
    let status = child
        .wait()
        .await
        .map_err(|e| json!({ "error": format!("failed to wait for cargo: {e}") }))?;
    let (stdout, out_truncated) = stdout_task
        .await
        .map_err(|e| json!({ "error": format!("stdout task failed: {e}") }))?;
    let (stderr, err_truncated) = stderr_task
        .await
        .map_err(|e| json!({ "error": format!("stderr task failed: {e}") }))?;
    Ok(CapturedOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        truncated: out_truncated || err_truncated,
    })
}

async fn read_lines(
    pipe: Option<impl tokio::io::AsyncRead + Unpin>,
    sink: ChunkSink,
    stream: &'static str,
    max_output: usize,
) -> (String, bool) {
    let Some(pipe) = pipe else {
        return (String::new(), false);
    };
    let mut captured = String::new();
    let mut truncated = false;
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        sink.chunk(json!({ "stream": stream, "line": line }));
        if captured.len() + line.len() + 1 <= max_output {
            captured.push_str(&line);
            captured.push('\n');
        } else {
            truncated = true;
        }
    }
    (captured, truncated)
}

pub(crate) fn summarize_failures(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with("error")
                || trimmed.starts_with("warning")
                || trimmed.starts_with("failures:")
                || trimmed.starts_with("thread '")
                || trimmed.contains("panicked at")
                || trimmed.contains("test result: FAILED")
        })
        .take(40)
        .map(ToOwned::to_owned)
        .collect()
}

pub(crate) fn targeted_test_hints(changed_files: &[&str]) -> Vec<String> {
    let mut hints = Vec::new();
    for file in changed_files {
        if let Some(name) = file
            .strip_prefix("crates/")
            .and_then(|rest| rest.split('/').next())
        {
            hints.push(format!("cargo test -p {name}"));
        } else if file.starts_with("tests/") {
            hints.push(format!("cargo test {}", file.trim_end_matches(".rs")));
        } else if file.ends_with(".rs") {
            hints.push("cargo test".to_string());
        }
    }
    hints.sort();
    hints.dedup();
    hints
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_failures_and_detects_targeted_tests() {
        let summary = summarize_failures(
            "ok\nerror[E0308]: mismatched types\nthread 'x' panicked at src/lib.rs:1\ntest result: FAILED\n",
        );
        assert_eq!(summary.len(), 3);
        assert!(summary[0].contains("E0308"));
        let hints = targeted_test_hints(&["crates/hugr-core/src/lib.rs", "tests/scripted.rs"]);
        assert_eq!(
            hints,
            vec![
                "cargo test -p hugr-core".to_string(),
                "cargo test tests/scripted".to_string(),
            ]
        );
        assert!(CargoVerify.runs_in_background());
    }
}
