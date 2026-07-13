//! External tools declared in the manifest reach a definition-run agent.
//!
//! A `[tools.mcp.<name>]` grant must spawn the stdio server, discover its tools,
//! and register them as ordinary capabilities on the assembled agent. We assert
//! the discovered tool shows up on the agent's `describe()` card — registration
//! is what makes it callable.

use huggr_toolkit::AgentDefinition;
use huggr_toolkit::runtime::build_agent;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[tokio::test]
async fn manifest_declared_mcp_tool_is_registered_on_the_agent() {
    if !python3_available() {
        eprintln!("skipping: python3 unavailable");
        return;
    }
    // A tiny stdio MCP server (same shape as the huggr-host C1 test), declared
    // entirely from the manifest.
    let manifest = r#"
[agent]
name = "mcp-agent"
[models]
default = "balanced"

[tools.mcp.fake]
command = "python3"
args = ["-u", "-c", '''
import json, sys
for line in sys.stdin:
    if not line.strip():
        continue
    msg = json.loads(line)
    if "id" not in msg:
        continue
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake-mcp", "version": "0"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo a message.", "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}}]}
    elif method == "tools/call":
        args = msg.get("params", {}).get("arguments", {})
        result = {"content": [{"type": "text", "text": "echo:" + str(args.get("message", ""))}], "isError": False}
    else:
        print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "unknown method"}}), flush=True)
        continue
    print(json.dumps({"jsonrpc": "2.0", "id": msg["id"], "result": result}), flush=True)
''']
"#;
    let def = AgentDefinition::parse(manifest, "huggr.toml").unwrap();
    let (agent, warnings) = build_agent(&def)
        .await
        .expect("MCP server should be loaded from the manifest");
    assert_eq!(warnings.len(), 1, "{warnings:?}");

    let card = agent.describe();
    let names: Vec<_> = card.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"mcp__fake__echo"),
        "manifest-declared MCP tool must be registered; got {names:?}"
    );
}
