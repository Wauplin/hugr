//! The universal CLI surface every built agent binary wraps (ROADMAP T2.1,
//! ARCHITECTURE §21.1).
//!
//! A binary produced by `hugr build --surface cli` embeds its definition as a
//! [`bundle`] and calls [`run_cli`] from `main`. Every built agent has the same
//! shape:
//!
//! ```text
//! <agent> "question" [--trace <id>] [--json|--pretty] [--blob <path>...]
//! <agent> --describe | --config | --traces
//! ```
//!
//! One JSON [`Answer`] on stdout, diagnostics on stderr, and **exit 0** for the
//! ask path — a bad manifest, a build failure, or an infra `AskError` all come
//! back as `status: "error"` answers (the `hugr-docs` contract, now universal).
//! The audit sub-commands (`--describe`/`--config`/`--traces`) are inspection
//! surfaces: they print JSON and exit non-zero on failure.

use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Parser;
use hugr_agent::{Agent, Answer, AnswerMeta, AnswerStatus, Ask, BlobHandle, BlobRef, TraceId};

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::runtime::build_agent;

/// The file inside a bundle that carries the manifest (used to resolve the
/// agent home before a full unpack).
const MANIFEST_NAME: &str = "hugr.toml";

/// Parsed universal-CLI arguments. `argv[0]` is the agent's own binary name.
#[derive(Parser, Debug)]
#[command(about = "A Hugr subagent. Ask it a question; errors are status:error answers.")]
struct SurfaceArgs {
    /// The question to ask (omit when using --describe/--config/--traces).
    question: Option<String>,
    /// Resume/fork from an existing trace id (writes a new child trace).
    #[arg(long)]
    trace: Option<String>,
    /// Emit compact single-line JSON (default is pretty-printed).
    #[arg(long)]
    json: bool,
    /// Pretty-print the JSON answer (the default).
    #[arg(long)]
    pretty: bool,
    /// Hand a local file in as an inbound blob (repeatable).
    #[arg(long = "blob")]
    blobs: Vec<PathBuf>,
    /// Print the agent card (name, tools, privileges, tiers, pricing, limits).
    #[arg(long)]
    describe: bool,
    /// Print the effective configuration with provenance and redaction.
    #[arg(long)]
    config: bool,
    /// Print the stored traces (header-only, with lineage).
    #[arg(long)]
    traces: bool,
}

/// Which audit view, if any, was requested.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ask,
    Describe,
    Config,
    Traces,
}

/// Entry point a built agent binary calls from `main`. Parses `std::env::args`,
/// runs the requested operation, prints to stdout, and returns the process exit
/// code. The ask path always returns 0.
pub async fn run_cli(bundle_bytes: &'static [u8]) -> i32 {
    let args = SurfaceArgs::parse();
    let started = Instant::now();
    let pretty = !args.json; // default pretty; --json forces compact

    let mode = if args.describe {
        Mode::Describe
    } else if args.config {
        Mode::Config
    } else if args.traces {
        Mode::Traces
    } else {
        Mode::Ask
    };

    // Prepare the agent home (unpack the embedded definition) and load it.
    let home = match prepare_home(bundle_bytes) {
        Ok(home) => home,
        Err(err) => {
            return audit_or_answer_error(
                mode,
                format!("preparing agent home: {err}"),
                started,
                pretty,
            );
        }
    };
    let def = match AgentDefinition::load(&home) {
        Ok(def) => def,
        Err(err) => {
            return audit_or_answer_error(
                mode,
                format!("loading definition: {err}"),
                started,
                pretty,
            );
        }
    };
    for warning in &def.warnings {
        eprintln!("warning: {}", warning.message);
    }

    let (agent, warnings) = match build_agent(&def).await {
        Ok(built) => built,
        Err(err) => {
            return audit_or_answer_error(mode, format!("assembling agent: {err}"), started, pretty);
        }
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    match mode {
        Mode::Describe => print_json_or_die(&agent.describe(), pretty),
        Mode::Config => print_json_or_die(&agent.config(), pretty),
        Mode::Traces => match agent.traces() {
            Ok(heads) => print_json_or_die(&heads, pretty),
            Err(err) => {
                eprintln!("error: listing traces: {err}");
                1
            }
        },
        Mode::Ask => run_ask(&agent, args, started, pretty).await,
    }
}

/// Run one ask and print its answer. Missing question, blob problems, and infra
/// `AskError`s all surface as `status: "error"` answers (exit 0).
async fn run_ask(agent: &Agent, args: SurfaceArgs, started: Instant, pretty: bool) -> i32 {
    let Some(question) = args.question else {
        return print_answer(
            &error_answer(
                "no question provided (use --describe/--config/--traces for the audit views)",
                started,
            ),
            pretty,
        );
    };

    let mut blobs = Vec::with_capacity(args.blobs.len());
    for path in &args.blobs {
        match blob_from_path(path) {
            Ok(handle) => blobs.push(handle),
            Err(err) => return print_answer(&error_answer(err, started), pretty),
        }
    }

    let mut ask = Ask::new(question).with_blobs(blobs);
    if let Some(trace) = args.trace {
        ask = ask.with_trace_id(TraceId::new(trace));
    }

    match agent.ask(ask).await {
        Ok(answer) => print_answer(&answer, pretty),
        Err(err) => print_answer(&error_answer(err.to_string(), started), pretty),
    }
}

/// Build an inbound [`BlobHandle`] from a local path, guessing its media type
/// from the extension. The name hint is the file's own name.
fn blob_from_path(path: &Path) -> Result<BlobHandle, String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| format!("blob path is not valid UTF-8: {}", path.display()))?;
    let media = media_type_for(path);
    let mut handle = BlobHandle::new(
        BlobRef::Path {
            path: path_str.to_string(),
        },
        media,
    );
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        handle = handle.with_name(name);
    }
    Ok(handle)
}

/// A best-effort media type from a file extension.
fn media_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("md" | "markdown") => "text/markdown",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        _ => "application/octet-stream",
    }
}

/// Resolve the per-agent home directory and unpack the embedded definition into
/// it. The definition source files are (re-)written every run — they are
/// immutable by design — while the runtime dirs (traces, scratch) are never in
/// the bundle, so persisted traces survive across runs and `--trace` resume
/// works on a machine with no repo checkout.
fn prepare_home(bundle_bytes: &[u8]) -> std::io::Result<PathBuf> {
    let manifest = bundle::get(bundle_bytes, MANIFEST_NAME)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "bundle has no hugr.toml")
    })?;
    let (name, version) = manifest_identity(&manifest);
    let home = agent_home_dir(&name, &version);
    std::fs::create_dir_all(&home)?;
    bundle::unpack(bundle_bytes, &home)?;
    Ok(home)
}

/// Pull `name`/`version` out of the manifest bytes with a forgiving parse — we
/// only need them to name the home dir, so a parse miss falls back to defaults.
fn manifest_identity(manifest: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(manifest);
    let value: toml::Value = text
        .parse()
        .unwrap_or(toml::Value::Table(Default::default()));
    let agent = value.get("agent");
    let name = agent
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("agent")
        .to_string();
    let version = agent
        .and_then(|a| a.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    (sanitize_segment(&name), sanitize_segment(&version))
}

/// The per-agent home: `$HUGR_AGENT_HOME` if set, else `<data>/hugr/<name>@<version>`
/// where `<data>` follows XDG (`$XDG_DATA_HOME`, else `$HOME/.local/share`, else
/// the temp dir). Stable across invocations so traces persist.
fn agent_home_dir(name: &str, version: &str) -> PathBuf {
    if let Ok(explicit) = std::env::var("HUGR_AGENT_HOME") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    data.join("hugr").join(format!("{name}@{version}"))
}

/// Reduce a manifest string to a single safe path segment.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "agent".to_string()
    } else {
        cleaned
    }
}

/// For an audit view, a preparation failure is a stderr error + non-zero exit;
/// for the ask path it is an error answer at exit 0 (§18.1).
fn audit_or_answer_error(mode: Mode, message: String, started: Instant, pretty: bool) -> i32 {
    if mode == Mode::Ask {
        print_answer(&error_answer(message, started), pretty)
    } else {
        eprintln!("error: {message}");
        1
    }
}

fn error_answer(message: impl Into<String>, started: Instant) -> Answer {
    let meta = AnswerMeta::new().with_duration_ms(started.elapsed().as_millis() as u64);
    Answer::new(
        AnswerStatus::Error,
        message.into(),
        TraceId::new(String::new()),
        meta,
    )
}

fn print_answer(answer: &Answer, pretty: bool) -> i32 {
    print_json_or_die(answer, pretty);
    0 // the ask path always exits 0 — errors are answers
}

/// Serialize to stdout. Returns exit 0 on success, 1 if serialization fails
/// (used directly by the audit views).
fn print_json_or_die<T: serde::Serialize>(value: &T, pretty: bool) -> i32 {
    let result = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match result {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(err) => {
            eprintln!("error: serializing output: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{Template, scaffold_files};

    /// Build a bundle in memory from a scaffolded definition written to a temp
    /// dir, so surface behavior is testable without `cargo build`.
    fn bundle_for(template: Template, name: &str) -> (Vec<u8>, PathBuf) {
        let root = std::env::temp_dir().join(format!("hugr-surface-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for file in scaffold_files(name, template) {
            let path = root.join(&file.rel_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file.contents).unwrap();
        }
        let bytes = bundle::pack(&root, &[".hugr-traces", ".scratch"]).unwrap();
        (bytes, root)
    }

    #[test]
    fn manifest_identity_reads_name_and_version() {
        let (id_name, id_version) =
            manifest_identity(b"[agent]\nname='my agent'\nversion='1.2.3'\n");
        assert_eq!(id_name, "my_agent");
        assert_eq!(id_version, "1.2.3");
        // Missing fields fall back.
        let (n, v) = manifest_identity(b"garbage = ");
        assert_eq!((n.as_str(), v.as_str()), ("agent", "0.0.0"));
    }

    #[tokio::test]
    async fn prepare_home_unpacks_and_agent_describes() {
        let (bytes, src) = bundle_for(Template::Blank, "surfacedesc");
        let home = std::env::temp_dir().join(format!("hugr-home-desc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // Point the home dir at a temp location via the runtime resolver.
        unsafe { std::env::set_var("HUGR_AGENT_HOME", &home) };
        let prepared = prepare_home(&bytes).unwrap();
        assert_eq!(prepared, home);
        assert!(home.join("hugr.toml").exists());

        let def = AgentDefinition::load(&home).unwrap();
        let (agent, _) = build_agent(&def).await.unwrap();
        let card = agent.describe();
        assert_eq!(card.name, "surfacedesc");
        let tools: Vec<_> = card.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tools.contains(&"scratch_write"), "{tools:?}");

        unsafe { std::env::remove_var("HUGR_AGENT_HOME") };
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&home);
    }
}
