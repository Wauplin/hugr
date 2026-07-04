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
use hugr_agent::{Answer, AnswerMeta, AnswerStatus, Ask, TraceId};
use hugr_toolkit::AgentDefinition;
use hugr_toolkit::build::{BuildOptions, Surface, build as run_build};
use hugr_toolkit::runtime::{build_agent, trace_store_for};
use hugr_toolkit::scaffold::{Template, write_scaffold};
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
    /// Compile a definition into a self-contained artifact (cli/crate/python).
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
    /// Target surface: cli | crate | python | mcp.
    #[arg(long, default_value = "cli")]
    surface: String,
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
    /// Emit the JSON answer (the default output; accepted for symmetry with
    /// built binaries).
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
    let Some(surface) = Surface::parse(&args.surface) else {
        eprintln!(
            "error: unknown surface `{}` (supported: cli, crate)",
            args.surface
        );
        std::process::exit(2);
    };
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    for warning in &def.warnings {
        eprintln!("warning: {}", warning.message);
    }

    let out_dir = args.out.unwrap_or_else(|| args.agent_dir.join("dist"));
    let opts = BuildOptions {
        out_dir,
        release: args.release,
    };

    eprintln!("building `{}` (surface={})…", def.agent.name, args.surface);
    match run_build(&def, surface, &opts) {
        Ok(outcome) => match outcome.binary {
            Some(binary) => {
                eprintln!("built {} ✓", binary.display());
                match surface {
                    Surface::Mcp => eprintln!(
                        "serve it: {} --mcp-serve  (register this stdio command in your MCP client)",
                        binary.display()
                    ),
                    _ => eprintln!(
                        "run it: {} \"<question>\"  (self-contained; no repo checkout needed)",
                        binary.display()
                    ),
                }
            }
            None => {
                eprintln!("generated crate at {} ✓", outcome.crate_dir.display());
                match surface {
                    Surface::Python => eprintln!(
                        "build a wheel: (cd {} && maturin build --release), then `pip install` it",
                        outcome.crate_dir.display()
                    ),
                    _ => eprintln!("depend on it by path from your orchestrator, then call `ask`"),
                }
            }
        },
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

async fn run(args: RunArgs) {
    let started = Instant::now();

    // A bad manifest is an error answer, not a panic (§21.1).
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => return print_error(err.to_string(), started),
    };
    for warning in &def.warnings {
        eprintln!("warning: {}", warning.message);
    }

    let (agent, warnings) = match build_agent(&def).await {
        Ok(built) => built,
        Err(err) => return print_error(err.to_string(), started),
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut ask = Ask::new(args.question);
    if let Some(trace) = args.trace {
        ask = ask.with_trace_id(TraceId::new(trace));
    }

    match agent.ask(ask).await {
        Ok(answer) => print_answer(&answer),
        // Infrastructure failure (unknown parent id, store write, …) → error
        // answer at the surface boundary (§18.1).
        Err(err) => print_error(err.to_string(), started),
    }
    // Silence the unused flag; output is always the JSON answer for now.
    let _ = args.json;
}

fn print_answer(answer: &Answer) {
    match serde_json::to_string_pretty(answer) {
        Ok(json) => println!("{json}"),
        Err(err) => eprintln!("error: serializing answer: {err}"),
    }
}

fn print_error(message: String, started: Instant) {
    let meta = AnswerMeta::new().with_duration_ms(started.elapsed().as_millis() as u64);
    let answer = Answer::new(
        AnswerStatus::Error,
        message,
        TraceId::new(String::new()),
        meta,
    );
    print_answer(&answer);
}
