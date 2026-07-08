//! `hugr` — the builder/interpreter CLI (ROADMAP T1.3+).
//!
//! `hugr run <agent-dir> "question" [--trace <id>] [--json]` loads a definition
//! folder, assembles the `hugr-agent` runtime, and executes one ask. Per the
//! universal CLI contract (ARCHITECTURE §21.1): the JSON `Answer` goes to
//! stdout, diagnostics to stderr, and the process always exits 0 — run failures
//! (and even a bad manifest) come back as `status: "error"` answers.

use std::path::PathBuf;
use std::time::Instant;

use clap::{Parser, Subcommand};
use hugr_agent::TraceId;
use hugr_toolkit::AgentDefinition;
use hugr_toolkit::build::{BuildOptions, build as run_build};
use hugr_toolkit::runtime::{build_agent, trace_store_for};
use hugr_toolkit::scaffold::{Template, write_scaffold};
use hugr_toolkit::surface::{error_answer, print_answer, run_ask};
use hugr_toolkit::traces::render_lineage;

#[derive(Parser)]
#[command(
    name = "hugr",
    about = "Build and run tiny, self-contained Hugr subagents."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interpret a definition folder and answer one question.
    Run(RunArgs),
    /// Scaffold a new definition folder from a template.
    New(NewArgs),
    /// Compile a definition into one self-contained CLI binary (also serves
    /// `--mcp-serve`).
    Build(BuildArgs),
    /// List an agent's stored traces as a lineage tree (or prune / size them).
    Traces(TracesArgs),
    /// Verify a stored trace replays bit-for-bit.
    Verify(TraceArgs),
    /// Replay a stored trace (optionally step-by-step).
    Replay(ReplayArgs),
}

#[derive(Parser)]
struct AgentArg {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
}

#[derive(Parser)]
struct TracesArgs {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
    /// Prune the store under the retention policy below (lineage stays closed).
    #[arg(long)]
    prune: bool,
    /// Report the store's on-disk size (trace count + bytes) and exit.
    #[arg(long)]
    size: bool,
    /// Prune policy: keep at most this many (most-recent) traces.
    #[arg(long)]
    keep_max: Option<usize>,
    /// Prune policy: drop traces older than this many seconds.
    #[arg(long)]
    max_age_secs: Option<u64>,
    /// Prune policy: pin a trace id so it (and its ancestors) is never dropped.
    #[arg(long = "pin")]
    pins: Vec<String>,
}

#[derive(Parser)]
struct TraceArgs {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
    /// The trace id to operate on.
    trace_id: String,
}

#[derive(Parser)]
struct ReplayArgs {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
    /// The trace id to replay.
    trace_id: String,
    /// Print each replayed event and the commands/log it produced.
    #[arg(long)]
    step: bool,
}

#[derive(Parser)]
struct BuildArgs {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
    /// Where to write the generated shim crate (built binary lands under its
    /// `target/`). Defaults to `<agent-dir>/dist`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Build in release mode.
    #[arg(long)]
    release: bool,
}

#[derive(Parser)]
struct NewArgs {
    /// Name of the agent (also the folder created under the current directory).
    name: String,
    /// Starting template: docs | sqlite | blank.
    #[arg(long, default_value = "docs")]
    template: String,
}

#[derive(Parser)]
struct RunArgs {
    /// Path to the agent definition folder (containing hugr.toml).
    agent_dir: PathBuf,
    /// The question to ask.
    question: String,
    /// Resume/fork from an existing trace id (writes a new child trace).
    #[arg(long)]
    trace: Option<String>,
    /// Emit compact single-line JSON (default is pretty-printed), matching the
    /// built binary's flag.
    #[arg(long)]
    json: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::New(args) => new(args),
        Command::Build(args) => build(args),
        Command::Traces(args) => traces(args),
        Command::Verify(args) => verify(args),
        Command::Replay(args) => replay(args),
    }
}

/// Load a definition folder's trace store, exiting non-zero on a bad manifest.
/// Trace tooling is a developer inspection surface (like `new`), not the
/// ask/answer contract.
fn load_store(agent_dir: &std::path::Path) -> hugr_agent::TraceStore {
    match AgentDefinition::load(agent_dir) {
        Ok(def) => trace_store_for(&def),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

fn traces(args: TracesArgs) {
    let store = load_store(&args.agent_dir);

    if args.size {
        match store.size() {
            Ok(size) => println!(
                "{} trace(s), {} bytes on disk",
                size.trace_count, size.total_bytes
            ),
            Err(err) => {
                eprintln!("error: sizing store: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    if args.prune {
        let mut policy = hugr_agent::PrunePolicy::new();
        if let Some(n) = args.keep_max {
            policy = policy.keep_max(n);
        }
        if let Some(secs) = args.max_age_secs {
            policy = policy.max_age_secs(secs);
        }
        for pin in &args.pins {
            policy = policy.pin(TraceId::new(pin.clone()));
        }
        match store.prune(&policy) {
            Ok(report) => {
                println!(
                    "pruned {} trace(s), kept {} (lineage closed), freed {} bytes",
                    report.pruned.len(),
                    report.kept.len(),
                    report.freed_bytes
                );
                for id in &report.pruned {
                    println!("  - {id}");
                }
            }
            Err(err) => {
                eprintln!("error: pruning store: {err}");
                std::process::exit(1);
            }
        }
        return;
    }

    match store.list() {
        Ok(heads) => println!("{}", render_lineage(&heads)),
        Err(err) => {
            eprintln!("error: listing traces: {err}");
            std::process::exit(1);
        }
    }
}

fn verify(args: TraceArgs) {
    let store = load_store(&args.agent_dir);
    let trace = match store.get(&TraceId::new(args.trace_id.clone())) {
        Ok(trace) => trace,
        Err(err) => {
            eprintln!("error: loading trace {}: {err}", args.trace_id);
            std::process::exit(1);
        }
    };
    match hugr_replay::verify(&trace) {
        Ok(_) => println!("{} verified ✓ (replays bit-for-bit)", args.trace_id),
        Err(err) => {
            eprintln!("{} FAILED verification: {err}", args.trace_id);
            std::process::exit(1);
        }
    }
}

fn replay(args: ReplayArgs) {
    let store = load_store(&args.agent_dir);
    let trace = match store.get(&TraceId::new(args.trace_id.clone())) {
        Ok(trace) => trace,
        Err(err) => {
            eprintln!("error: loading trace {}: {err}", args.trace_id);
            std::process::exit(1);
        }
    };
    if args.step {
        let mut inspector = hugr_replay::Inspector::new(&trace);
        let total = inspector.len();
        while let Some(step) = inspector.step() {
            println!(
                "[{}/{}] event={} → {} command(s), {} log entr(ies)",
                step.index + 1,
                total,
                event_kind(&step.event),
                step.commands.len(),
                step.appended.len(),
            );
        }
        println!("replayed {total} event(s)");
    } else {
        let steps = hugr_replay::Inspector::new(&trace).run();
        let commands: usize = steps.iter().map(|s| s.commands.len()).sum();
        println!(
            "replayed {} event(s), {} command(s), {} log entr(ies)",
            steps.len(),
            commands,
            trace.log.len(),
        );
    }
}

/// A short label for a recorded event, for `--step` output.
fn event_kind(event: &hugr_core::Event) -> String {
    // Event is #[non_exhaustive]; its Debug is stable enough for a one-word tag.
    let dbg = format!("{event:?}");
    dbg.split(['{', '(', ' '])
        .next()
        .unwrap_or("Event")
        .to_string()
}

/// `hugr new` writes to stderr and sets a non-zero exit on failure — it is a
/// developer scaffolding command, not the ask/answer contract surface.
fn new(args: NewArgs) {
    let Some(template) = Template::parse(&args.template) else {
        eprintln!(
            "error: unknown template `{}` (expected docs | sqlite | blank)",
            args.template
        );
        std::process::exit(2);
    };
    match write_scaffold(std::path::Path::new("."), &args.name, template) {
        Ok(dir) => {
            eprintln!("created {} ({} template)", dir.display(), template.as_str());
            eprintln!(
                "next: export your provider key, then `hugr run {} \"<question>\"`",
                dir.display()
            );
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

/// `hugr build` is a developer command (like `new`): progress on stderr,
/// non-zero exit on failure — not the ask/answer contract surface.
fn build(args: BuildArgs) {
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let out_dir = args.out.unwrap_or_else(|| args.agent_dir.join("dist"));
    let opts = BuildOptions {
        out_dir,
        release: args.release,
    };

    eprintln!("building `{}`…", def.agent.name);
    match run_build(&def, &opts) {
        Ok(outcome) => {
            eprintln!("built {} ✓", outcome.binary.display());
            eprintln!(
                "run it: {} \"<question>\"  (self-contained; no repo checkout needed)",
                outcome.binary.display()
            );
            eprintln!(
                "serve MCP: {} --mcp-serve  (register this stdio command in your MCP client)",
                outcome.binary.display()
            );
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

async fn run(args: RunArgs) {
    let started = Instant::now();
    let pretty = !args.json;

    // A bad manifest is an error answer, not a panic (§21.1) — shared with the
    // built binary via `surface::error_answer`/`print_answer`.
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            print_answer(&error_answer(err.to_string(), started), pretty);
            return;
        }
    };
    let (agent, warnings) = match build_agent(&def).await {
        Ok(built) => built,
        Err(err) => {
            print_answer(&error_answer(err.to_string(), started), pretty);
            return;
        }
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    // The same run path as the built binary (ARCHITECTURE §21.1).
    run_ask(
        &agent,
        Some(args.question),
        args.trace,
        &[],
        started,
        pretty,
    )
    .await;
}
