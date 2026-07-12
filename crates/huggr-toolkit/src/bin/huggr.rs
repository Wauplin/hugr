//! `huggr` — the builder/interpreter CLI.
//!
//! `huggr run <agent-dir> "question" [--trace <id>] [--json]` loads an agent crate folder, assembles the `huggr-agent` runtime, and executes one ask. Per the universal CLI contract: the JSON `Answer` goes to stdout, diagnostics to stderr, and the process always exits 0 — run failures (and even a bad manifest) come back as `status: "error"` answers.

use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Parser, Subcommand};
use huggr_agent::{StatsOptions, TraceId};
use huggr_toolkit::AgentDefinition;
use huggr_toolkit::build::{BuildOptions, build as run_build};
use huggr_toolkit::build_python::build_python;
use huggr_toolkit::runtime::{build_agent, trace_store_for};
use huggr_toolkit::scaffold::{Template, write_scaffold};
use huggr_toolkit::stats::render_stats;
use huggr_toolkit::surface::{error_answer, print_answer, run_definition_args};
use huggr_toolkit::traces::render_lineage_with_feedback;

#[derive(Parser)]
#[command(name = "huggr", about = "Build and run tiny, self-contained huglets.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Interpret an agent crate folder and answer one question.
    Run(RunArgs),
    /// Scaffold a new agent crate folder from a template.
    New(NewArgs),
    /// Compile an agent crate into one self-contained CLI binary (also serves
    /// `--mcp-serve`).
    Build(BuildArgs),
    /// List an agent's stored traces as a lineage tree.
    Traces(TracesArgs),
    /// Aggregate an agent's stored trace analytics.
    Stats(StatsArgs),
    /// Run configured cron jobs until stopped.
    Cron(CronArgs),
    /// Verify a stored trace replays bit-for-bit.
    Verify(TraceArgs),
    /// Replay a stored trace (optionally step-by-step).
    Replay(ReplayArgs),
}

#[derive(Parser)]
struct AgentArg {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
}

#[derive(Parser)]
struct TracesArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
}

#[derive(Parser)]
struct StatsArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// Include traces at or after this trace's creation point.
    #[arg(long)]
    since: Option<String>,
    /// Report one trace only.
    #[arg(long)]
    trace: Option<String>,
    /// Emit JSON instead of a compact table.
    #[arg(long)]
    json: bool,
}

#[derive(Parser)]
struct CronArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// Allow cron jobs without max_cost_micro_usd.
    #[arg(long)]
    allow_uncapped: bool,
}

#[derive(Parser)]
struct TraceArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// The trace id to operate on.
    trace_id: String,
}

#[derive(Parser)]
struct ReplayArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// The trace id to replay.
    trace_id: String,
    /// Print each replayed event and the commands/log it produced.
    #[arg(long)]
    step: bool,
}

#[derive(Parser)]
struct BuildArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// Where to write the generated shim crate (built binary lands under its
    /// `target/`). Defaults to `<agent-dir>/dist`.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Build in release mode.
    #[arg(long)]
    release: bool,
    /// Surface(s) to generate: `cli` (default) and/or `python`. Repeatable or
    /// comma-separated (`--surface python`, `--surface cli,python`). The
    /// `python` surface also builds the CLI binary (it reads the response schema
    /// from it).
    #[arg(long, value_delimiter = ',', default_value = "cli")]
    surface: Vec<String>,
}

#[derive(Parser)]
struct NewArgs {
    /// Name of the agent (also the folder created under the current directory).
    name: String,
    /// Starting template: weather | blank.
    #[arg(long, default_value = "weather")]
    template: String,
}

#[derive(Parser)]
struct RunArgs {
    /// Path to the agent crate folder (containing Cargo.toml and huggr.toml).
    agent_dir: PathBuf,
    /// Arguments passed to the agent's generated surface.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    args: Vec<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run(args).await,
        Command::New(args) => new(args),
        Command::Build(args) => build(args),
        Command::Traces(args) => traces(args).await,
        Command::Stats(args) => stats(args).await,
        Command::Cron(args) => cron(args).await,
        Command::Verify(args) => verify(args),
        Command::Replay(args) => replay(args),
    }
}

/// Load an agent crate folder's trace store, exiting non-zero on a bad manifest.
/// Trace tooling is a developer inspection surface (like `new`), not the
/// ask/answer contract.
fn load_store(agent_dir: &std::path::Path) -> huggr_agent::TraceStore {
    match AgentDefinition::load(agent_dir) {
        Ok(def) => trace_store_for(&def),
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

async fn traces(args: TracesArgs) {
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let (agent, warnings) = match build_agent(&def).await {
        Ok(result) => result,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    match agent.traces_with_feedback().await {
        Ok(heads) => println!("{}", render_lineage_with_feedback(&heads)),
        Err(err) => {
            eprintln!("error: listing traces: {err}");
            std::process::exit(1);
        }
    }
}

async fn stats(args: StatsArgs) {
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let (agent, warnings) = match build_agent(&def).await {
        Ok(result) => result,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }

    let mut options = StatsOptions::new();
    if let Some(trace_id) = args.since {
        options = options.since(TraceId::new(trace_id));
    }
    if let Some(trace_id) = args.trace {
        options = options.trace(TraceId::new(trace_id));
    }
    let stats = match agent.stats(options).await {
        Ok(stats) => stats,
        Err(err) => {
            eprintln!("error: computing stats: {err}");
            std::process::exit(1);
        }
    };
    if args.json {
        match serde_json::to_string_pretty(&stats) {
            Ok(json) => println!("{json}"),
            Err(err) => {
                eprintln!("error: serializing stats: {err}");
                std::process::exit(1);
            }
        }
    } else {
        println!("{}", render_stats(&stats));
    }
}

async fn cron(args: CronArgs) {
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    };
    let code =
        huggr_toolkit::cron::serve_definition(def, Default::default(), args.allow_uncapped).await;
    if code != 0 {
        std::process::exit(code);
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
    match huggr_replay::verify(&trace) {
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
        let mut inspector = huggr_replay::Inspector::new(&trace);
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
        let steps = huggr_replay::Inspector::new(&trace).run();
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
fn event_kind(event: &huggr_core::Event) -> String {
    // Event is #[non_exhaustive]; its Debug is stable enough for a one-word tag.
    let dbg = format!("{event:?}");
    dbg.split(['{', '(', ' '])
        .next()
        .unwrap_or("Event")
        .to_string()
}

/// `huggr new` writes to stderr and sets a non-zero exit on failure — it is a
/// developer scaffolding command, not the ask/answer contract surface.
fn new(args: NewArgs) {
    let Some(template) = Template::parse(&args.template) else {
        eprintln!(
            "error: unknown template `{}` (expected weather | blank)",
            args.template
        );
        std::process::exit(2);
    };
    match write_scaffold(std::path::Path::new("."), &args.name, template) {
        Ok(dir) => {
            eprintln!("created {} ({} template)", dir.display(), template.as_str());
            eprintln!(
                "next: export your provider key, then `huggr run {} \"<question>\"`",
                dir.display()
            );
        }
        Err(err) => {
            eprintln!("error: {err}");
            std::process::exit(1);
        }
    }
}

/// `huggr build` is a developer command (like `new`): progress on stderr,
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

    // Validate the requested surfaces up front (open set today: cli, python).
    let mut python = false;
    let mut cli = false;
    for surface in &args.surface {
        match surface.as_str() {
            "cli" => cli = true,
            "python" => python = true,
            other => {
                eprintln!("error: unknown surface `{other}` (expected cli | python)");
                std::process::exit(2);
            }
        }
    }

    eprintln!("building `{}`…", def.agent.name);
    // The Python surface builds the CLI binary internally (it reads the response
    // schema from `--config`), so requesting python covers cli too.
    if python {
        match build_python(&def, &opts) {
            Ok(outcome) => {
                eprintln!("built python surface ✓ ({})", outcome.crate_dir.display());
                match &outcome.wheel {
                    Some(wheel) => eprintln!(
                        "install it: pip install {}  (then `import {}`)",
                        wheel.display(),
                        outcome.module
                    ),
                    None => eprintln!(
                        "build the wheel: (cd {} && maturin build --release)",
                        outcome.crate_dir.display()
                    ),
                }
            }
            Err(err) => {
                eprintln!("error: {err}");
                std::process::exit(1);
            }
        }
    } else if cli {
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
}

async fn run(args: RunArgs) {
    let started = Instant::now();
    let pretty = !args.args.iter().any(|arg| arg == "--json");

    // A bad manifest is an error answer, not a panic — shared with the
    // built binary via `surface::error_answer`/`print_answer`.
    let def = match AgentDefinition::load(&args.agent_dir) {
        Ok(def) => def,
        Err(err) => {
            print_answer(&error_answer(err.to_string(), started), pretty);
            return;
        }
    };
    if def.response_schema.is_none() {
        run_typed_definition(&args.agent_dir, &def, &args.args, started, pretty);
        return;
    }
    // The same generated surface as the built binary, including
    // agent-specific runtime arguments.
    run_definition_args(def, args.args, started).await;
}

/// Generic `huggr run` cannot link arbitrary agent crates into the already-built
/// toolkit binary. For typed response agents, run the same generated shim
/// as `huggr build`.
fn run_typed_definition(
    agent_dir: &Path,
    def: &AgentDefinition,
    argv: &[String],
    started: Instant,
    pretty: bool,
) {
    let out_dir = typed_run_out_dir(agent_dir, &def.agent.name);
    let opts = BuildOptions {
        out_dir,
        release: false,
    };
    let outcome = match run_build(def, &opts) {
        Ok(outcome) => outcome,
        Err(err) => {
            print_answer(&error_answer(err.to_string(), started), pretty);
            return;
        }
    };
    let status = std::process::Command::new(&outcome.binary)
        .args(argv)
        .status();
    if let Err(err) = status {
        print_answer(
            &error_answer(
                format!(
                    "running typed response shim {}: {err}",
                    outcome.binary.display()
                ),
                started,
            ),
            pretty,
        );
    }
}

fn typed_run_out_dir(agent_dir: &Path, agent_name: &str) -> PathBuf {
    let key = agent_dir
        .canonicalize()
        .unwrap_or_else(|_| agent_dir.to_path_buf());
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut hasher);
    let hash = hasher.finish();
    let safe_name: String = agent_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    std::env::temp_dir()
        .join("huggr-run")
        .join(format!("{safe_name}-{hash:016x}"))
}
