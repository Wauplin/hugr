//! Repo-orientation capabilities for coding-agent sessions (ROADMAP_2 D1).
//!
//! These are ordinary read-only host capabilities. The brain sees only their
//! schemas and opaque JSON results; all filesystem/process IO stays here.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use serde_json::json;
use tokio::process::Command;

use crate::capability::{Capability, ChunkSink};

const DEFAULT_MAX_FILES: usize = 2_000;
const DEFAULT_MAX_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_LINES: usize = 200;

pub struct RepoFiles;
pub struct RepoSearch;
pub struct RepoRead;
pub struct GitStatus;
pub struct GitDiff;
pub struct GitLog;
pub struct PackageMetadata;

#[async_trait]
impl Capability for RepoFiles {
    fn name(&self) -> &str {
        "repo_files"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "repo_files",
            "List repository files quickly, skipping heavy directories such as .git and target.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Repository root. Defaults to current directory." },
                    "max_files": { "type": "integer", "description": "Maximum number of files to return." }
                }
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let max_files = usize_arg(&args, "max_files").unwrap_or(DEFAULT_MAX_FILES);
        let root_for_walk = root.clone();
        let files = tokio::task::spawn_blocking(move || list_files(&root_for_walk, max_files))
            .await
            .map_err(|e| json!({ "error": format!("file listing task failed: {e}") }))??;
        Ok(json!({
            "root": root.display().to_string(),
            "files": files,
            "truncated": files.len() >= max_files,
        }))
    }
}

#[async_trait]
impl Capability for RepoSearch {
    fn name(&self) -> &str {
        "repo_search"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "repo_search",
            "Search repository text with ripgrep. Returns matching lines with file and line numbers.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Regex/text pattern for rg." },
                    "root": { "type": "string", "description": "Directory to search. Defaults to current directory." },
                    "glob": { "type": "string", "description": "Optional rg glob, e.g. '*.rs'." },
                    "max_matches": { "type": "integer", "description": "Maximum matches to return." }
                },
                "required": ["query"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let query = string_arg(&args, "query")?;
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let max_matches = usize_arg(&args, "max_matches").unwrap_or(200);
        let mut cmd = Command::new("rg");
        cmd.arg("--line-number")
            .arg("--column")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--max-count")
            .arg(max_matches.to_string());
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            cmd.arg("--glob").arg(glob);
        }
        cmd.arg(query).arg(&root);
        command_json("rg", cmd).await
    }
}

#[async_trait]
impl Capability for RepoRead {
    fn name(&self) -> &str {
        "repo_read"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "repo_read",
            "Read a targeted UTF-8 file slice by line range.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Path to the file to read." },
                    "start_line": { "type": "integer", "description": "1-based start line. Defaults to 1." },
                    "max_lines": { "type": "integer", "description": "Maximum lines to return." },
                    "max_bytes": { "type": "integer", "description": "Maximum bytes to return." }
                },
                "required": ["path"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let path = path_arg(&args, "path")
            .ok_or_else(|| json!({ "error": "missing string argument `path`" }))?;
        let start_line = usize_arg(&args, "start_line").unwrap_or(1).max(1);
        let max_lines = usize_arg(&args, "max_lines").unwrap_or(DEFAULT_MAX_LINES);
        let max_bytes = usize_arg(&args, "max_bytes").unwrap_or(DEFAULT_MAX_BYTES);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| json!({ "error": format!("failed to read {}: {e}", path.display()) }))?;
        let mut out = String::new();
        let mut returned = 0usize;
        let mut truncated = false;
        for (idx, line) in content.lines().enumerate().skip(start_line - 1) {
            if returned >= max_lines {
                truncated = true;
                break;
            }
            let candidate = format!("{}\t{}\n", idx + 1, line);
            if out.len() + candidate.len() > max_bytes {
                truncated = true;
                break;
            }
            out.push_str(&candidate);
            returned += 1;
        }
        Ok(json!({
            "path": path.display().to_string(),
            "start_line": start_line,
            "lines_returned": returned,
            "content": out,
            "truncated": truncated,
        }))
    }
}

#[async_trait]
impl Capability for GitStatus {
    fn name(&self) -> &str {
        "git_status"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "git_status",
            "Show concise git working tree status.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Repository root. Defaults to current directory." }
                }
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let mut cmd = git_cmd(&root);
        cmd.args(["status", "--short", "--branch"]);
        command_json("git_status", cmd).await
    }
}

#[async_trait]
impl Capability for GitDiff {
    fn name(&self) -> &str {
        "git_diff"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "git_diff",
            "Show git diff for the worktree, index, or a specific path.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Repository root. Defaults to current directory." },
                    "cached": { "type": "boolean", "description": "Show staged diff." },
                    "path": { "type": "string", "description": "Optional pathspec." }
                }
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let mut cmd = git_cmd(&root);
        cmd.arg("diff").arg("--no-color");
        if args.get("cached").and_then(Value::as_bool).unwrap_or(false) {
            cmd.arg("--cached");
        }
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            cmd.arg("--").arg(path);
        }
        command_json("git_diff", cmd).await
    }
}

#[async_trait]
impl Capability for GitLog {
    fn name(&self) -> &str {
        "git_log"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "git_log",
            "Show recent git commits in concise one-line form.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Repository root. Defaults to current directory." },
                    "max_count": { "type": "integer", "description": "Maximum commits to return." }
                }
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let max_count = usize_arg(&args, "max_count").unwrap_or(20);
        let mut cmd = git_cmd(&root);
        cmd.args(["log", "--oneline", "--decorate", "--max-count"])
            .arg(max_count.to_string());
        command_json("git_log", cmd).await
    }
}

#[async_trait]
impl Capability for PackageMetadata {
    fn name(&self) -> &str {
        "package_metadata"
    }

    fn requires_permission(&self) -> bool {
        false
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "package_metadata",
            "Read Rust package/workspace metadata using cargo metadata --no-deps.",
            json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string", "description": "Directory containing Cargo.toml. Defaults to current directory." }
                }
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let root = path_arg(&args, "root").unwrap_or_else(|| PathBuf::from("."));
        let mut cmd = Command::new("cargo");
        cmd.current_dir(root)
            .args(["metadata", "--format-version", "1", "--no-deps"]);
        command_json("package_metadata", cmd).await
    }
}

fn string_arg(args: &Value, name: &str) -> Result<String, Value> {
    args.get(name)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| json!({ "error": format!("missing string argument `{name}`") }))
}

fn path_arg(args: &Value, name: &str) -> Option<PathBuf> {
    args.get(name).and_then(Value::as_str).map(PathBuf::from)
}

fn usize_arg(args: &Value, name: &str) -> Option<usize> {
    args.get(name)
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

fn list_files(root: &Path, max_files: usize) -> Result<Vec<String>, Value> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| json!({ "error": format!("failed to list {}: {e}", dir.display()) }))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| json!({ "error": format!("failed to read dir entry: {e}") }))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if matches!(
                name.as_ref(),
                ".git" | "target" | "node_modules" | ".next" | "dist"
            ) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                let rel = path.strip_prefix(root).unwrap_or(&path);
                files.push(rel.display().to_string());
                if files.len() >= max_files {
                    files.sort();
                    return Ok(files);
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

fn git_cmd(root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(root);
    cmd
}

async fn command_json(label: &str, mut cmd: Command) -> Result<Value, Value> {
    let output = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| json!({ "error": format!("{label} failed to spawn: {e}") }))?;
    Ok(json!({
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hugr_core::{Event, OpId};
    use tokio::sync::mpsc;

    use super::*;
    use crate::test_support::TempDir;

    #[tokio::test]
    async fn repo_orientation_tools_are_read_only_and_list_files() {
        let root = TempDir::new("repo-files");
        let root = root.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn demo() {}\n").unwrap();
        std::fs::write(root.join("target/ignored.txt"), "ignored\n").unwrap();

        let tools: Vec<Arc<dyn Capability>> = vec![
            Arc::new(RepoFiles),
            Arc::new(RepoSearch),
            Arc::new(RepoRead),
            Arc::new(GitStatus),
            Arc::new(GitDiff),
            Arc::new(GitLog),
            Arc::new(PackageMetadata),
        ];
        let names: Vec<_> = tools.iter().map(|tool| tool.schema().name).collect();
        assert_eq!(
            names,
            vec![
                "repo_files",
                "repo_search",
                "repo_read",
                "git_status",
                "git_diff",
                "git_log",
                "package_metadata",
            ]
        );
        assert!(tools.iter().all(|tool| !tool.requires_permission()));

        let (tx, _rx) = mpsc::unbounded_channel::<Event>();
        let sink = ChunkSink::new(OpId(0), tx);
        let result = RepoFiles
            .invoke(json!({ "root": root, "max_files": 10 }), &sink)
            .await
            .unwrap();
        let files = result["files"].as_array().unwrap();
        assert!(files.iter().any(|f| f == "Cargo.toml"));
        assert!(files.iter().any(|f| f == "src/lib.rs"));
        assert!(!files.iter().any(|f| f == "target/ignored.txt"));
    }
}
