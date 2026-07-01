//! An example **third-party** Baton plugin (ROADMAP Phase 5).
//!
//! This is a standalone program in its own crate that depends on *nothing* from
//! Baton — only `serde_json`. It speaks the documented plugin protocol over
//! stdio: read one JSON request per line on stdin, write JSON responses on
//! stdout. That is the whole contract; the host loads it via
//! `baton_plugin_abi::SubprocessPlugin` and surfaces its tools as ordinary
//! capabilities, with no recompile of the core.
//!
//! It provides two tools:
//! - `uppercase` — returns its `text` argument upper-cased (with a progress chunk);
//! - `reverse`   — returns its `text` argument reversed.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

const PROTOCOL_VERSION: u32 = 1;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    // One request per line; each spawn handles a single request then hits EOF.
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            emit(&stdout, &error("malformed request JSON"));
            continue;
        };

        match request.get("req").and_then(Value::as_str) {
            Some("describe") => emit(&stdout, &describe()),
            Some("invoke") => {
                let name = request.get("name").and_then(Value::as_str).unwrap_or("");
                let args = request.get("args").cloned().unwrap_or_else(|| json!({}));
                invoke(&stdout, name, &args);
            }
            // `on_event` (reserved) and anything else: nothing to do.
            _ => {}
        }
    }
}

/// The tools this plugin provides.
fn describe() -> Value {
    json!({
        "kind": "description",
        "protocol_version": PROTOCOL_VERSION,
        "tools": [
            {
                "name": "uppercase",
                "description": "Return the given text upper-cased.",
                "parameters": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            },
            {
                "name": "reverse",
                "description": "Return the given text reversed.",
                "parameters": {
                    "type": "object",
                    "properties": { "text": { "type": "string" } },
                    "required": ["text"]
                }
            }
        ]
    })
}

/// Run one tool: stream an optional progress chunk, then a terminal result/error.
fn invoke(stdout: &std::io::Stdout, name: &str, args: &Value) {
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    match name {
        "uppercase" => {
            // Demonstrate streaming: a progress chunk before the result.
            emit(
                stdout,
                &json!({ "kind": "chunk", "value": { "progress": "uppercasing" } }),
            );
            emit(
                stdout,
                &json!({ "kind": "result", "value": { "text": text.to_uppercase() } }),
            );
        }
        "reverse" => {
            let reversed: String = text.chars().rev().collect();
            emit(
                stdout,
                &json!({ "kind": "result", "value": { "text": reversed } }),
            );
        }
        other => emit(stdout, &error(&format!("unknown tool: {other}"))),
    }
}

fn error(message: &str) -> Value {
    json!({ "kind": "error", "value": { "error": message } })
}

/// Write one response as a single JSON line.
fn emit(stdout: &std::io::Stdout, value: &Value) {
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{value}");
    let _ = lock.flush();
}
