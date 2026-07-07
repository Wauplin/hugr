//! T2.5 — surface conformance suite.
//!
//! One scripted scenario — **ask → follow-up → fork → describe** — run against
//! every surface of the *same* definition, asserting identical behavior modulo
//! transport (ARCHITECTURE §21). Because every surface is a thin serialization
//! of the one `hugr-agent` API, they must agree on: the agent card (name +
//! tool set), the status of each ask, and the trace-lineage relationships
//! (a follow-up and a fork each write fresh, distinct trace ids).
//!
//! The whole suite is `#[ignore]`d: it compiles a real cli/mcp binary, which
//! is slow. Run it with `cargo test -p hugr-toolkit --test conformance --
//! --ignored`. It is the gate for `hugr build` changes.
//!
//! No network: with no reachable model every ask is a `status: "error"` answer,
//! but a trace still persists — so the *lineage* invariants (the point of the
//! conformance check) hold identically across surfaces.

use std::path::Path;
use std::process::Command;

use hugr_agent::Ask;
use hugr_toolkit::build::{BuildOptions, build};
use hugr_toolkit::manifest::AgentDefinition;
use hugr_toolkit::runtime::build_agent;
use hugr_toolkit::scaffold::{Template, scaffold_files};

/// The normalized, transport-independent result of the scenario. Every surface
/// must produce an equal value.
#[derive(Debug, PartialEq, Eq)]
struct Normalized {
    describe_name: String,
    tools: Vec<String>,
    statuses: Vec<String>,
    distinct_traces: bool,
}

/// Write the shared definition folder used by all surfaces.
fn write_def(root: &Path, name: &str) {
    let _ = std::fs::remove_dir_all(root);
    for file in scaffold_files(name, Template::Docs) {
        let path = root.join(&file.rel_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, file.contents).unwrap();
    }
}

#[test]
#[ignore = "compiles real artifacts; slow"]
fn every_surface_agrees_on_the_scenario() {
    let base = std::env::temp_dir().join(format!("hugr-conf-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let def_dir = base.join("agent");
    write_def(&def_dir, "conform");
    let def = AgentDefinition::load(&def_dir).unwrap();

    // The reference result: the in-process typed Agent (this IS the `crate`
    // surface's semantics — a direct `Agent::ask`, no serialization).
    let reference = reference_result(&def, &base.join("home-ref"));
    println!("reference: {reference:?}");

    // Build the cli/mcp binary once (the mcp surface is the same artifact).
    let binary = build(
        &def,
        &BuildOptions {
            out_dir: base.join("dist"),
            release: false,
        },
    )
    .expect("cli build")
    .binary;

    let cli = cli_result(&binary, &base.join("home-cli"));
    assert_eq!(cli, reference, "cli surface diverges from reference");

    let mcp = mcp_result(&binary, &base.join("home-mcp"));
    assert_eq!(mcp, reference, "mcp surface diverges from reference");

    let _ = std::fs::remove_dir_all(&base);
}

/// The scenario over the in-process typed agent (the `crate` surface).
fn reference_result(def: &AgentDefinition, home: &Path) -> Normalized {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        // Point the agent's stores under a private home for isolation.
        let mut def = def.clone();
        def.traces.store = Some(home.join("traces").to_string_lossy().into_owned());
        def.scratchpad.root = Some(home.join("scratch").to_string_lossy().into_owned());
        let (agent, _) = build_agent(&def).await.unwrap();

        let a1 = agent.ask(Ask::new("q1")).await.unwrap();
        let a2 = agent
            .ask(Ask::new("q2").with_trace_id(a1.trace_id.clone()))
            .await
            .unwrap();
        let a3 = agent
            .ask(Ask::new("q3").with_trace_id(a1.trace_id.clone()))
            .await
            .unwrap();
        // Serialize through the wire form so the reference compares field-for-
        // field with the serialized surfaces.
        let describe = serde_json::to_value(agent.describe()).unwrap();
        let a1 = serde_json::to_value(&a1).unwrap();
        let a2 = serde_json::to_value(&a2).unwrap();
        let a3 = serde_json::to_value(&a3).unwrap();
        normalized_from(&describe, &[&a1, &a2, &a3])
    })
}

/// The scenario over the built binary's CLI surface.
fn cli_result(binary: &Path, home: &Path) -> Normalized {
    let describe = run_json(binary, &["--describe"], home);
    let a1 = run_json(binary, &["q1", "--json"], home);
    let t1 = trace_of(&a1);
    let a2 = run_json(binary, &["q2", "--trace", &t1, "--json"], home);
    let a3 = run_json(binary, &["q3", "--trace", &t1, "--json"], home);
    normalized_from(&describe, &[&a1, &a2, &a3])
}

/// The scenario over the built binary's `--mcp-serve` stdio surface.
fn mcp_result(binary: &Path, home: &Path) -> Normalized {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(binary)
        .arg("--mcp-serve")
        .env("HUGR_AGENT_HOME", home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut rpc = |line: String| -> serde_json::Value {
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let mut resp = String::new();
        stdout.read_line(&mut resp).unwrap();
        serde_json::from_str(&resp).unwrap()
    };

    let init = rpc(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into());
    let name = init["result"]["serverInfo"]["name"]
        .as_str()
        .unwrap()
        .to_string();
    let list = rpc(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.into());
    // The mcp surface exposes the single `ask` tool; the agent's own tool set is
    // reported by --describe, which shares the card. Use --describe for parity.
    let _ = list;

    let ask = |id: u32,
               q: &str,
               trace: Option<&str>,
               rpc: &mut dyn FnMut(String) -> serde_json::Value| {
        let args = match trace {
            Some(t) => format!(r#"{{"question":"{q}","trace_id":"{t}"}}"#),
            None => format!(r#"{{"question":"{q}"}}"#),
        };
        rpc(format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"ask","arguments":{args}}}}}"#
        ))["result"]["structuredContent"]
            .clone()
    };
    let a1 = ask(3, "q1", None, &mut rpc);
    let t1 = trace_of(&a1);
    let a2 = ask(4, "q2", Some(&t1), &mut rpc);
    let a3 = ask(5, "q3", Some(&t1), &mut rpc);

    drop(stdin);
    let _ = child.wait();

    // The mcp server reports its tool set via the shared card (--describe). Run
    // it separately for the tools list so all surfaces compare the same field.
    let describe = run_json(binary, &["--describe"], home);
    let mut result = normalized_from(&describe, &[&a1, &a2, &a3]);
    result.describe_name = name; // also assert the serverInfo name matches
    result
}

// --- helpers ---

fn normalized_from(describe: &serde_json::Value, answers: &[&serde_json::Value]) -> Normalized {
    let mut tools: Vec<String> = describe["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect();
    tools.sort();
    let traces: Vec<String> = answers.iter().map(|a| trace_of(a)).collect();
    let trace_refs: Vec<&str> = traces.iter().map(String::as_str).collect();
    Normalized {
        describe_name: describe["name"].as_str().unwrap().to_string(),
        tools,
        statuses: answers
            .iter()
            .map(|a| a["status"].as_str().unwrap().to_string())
            .collect(),
        distinct_traces: distinct(&trace_refs),
    }
}

fn trace_of(answer: &serde_json::Value) -> String {
    answer["trace_id"].as_str().unwrap_or_default().to_string()
}

fn distinct(ids: &[&str]) -> bool {
    let mut seen = std::collections::HashSet::new();
    ids.iter().all(|id| !id.is_empty() && seen.insert(*id))
}

fn run_json(binary: &Path, args: &[&str], home: &Path) -> serde_json::Value {
    let out = Command::new(binary)
        .args(args)
        .env("HUGR_AGENT_HOME", home)
        .output()
        .expect("run binary");
    assert!(out.status.success(), "exit 0 for {args:?}");
    serde_json::from_slice(&out.stdout).expect("json stdout")
}
