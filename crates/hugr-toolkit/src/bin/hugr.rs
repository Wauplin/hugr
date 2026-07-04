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
use hugr_toolkit::runtime::build_agent;
use hugr_toolkit::scaffold::{Template, write_scaffold};

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
    }
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

    let (agent, warnings) = match build_agent(&def) {
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
