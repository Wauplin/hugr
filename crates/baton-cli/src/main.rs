//! `baton` — the batteries-included showcase CLI.
//!
//! The engine setup below is the "CLI on a laptop" host: ~10 lines on top of
//! `baton-host` (ROADMAP Phase 1 exit criterion).
//!
//! Beyond running sessions, the CLI can **record** a session to a trace
//! (`--record <path>`) and **replay** one (`baton replay <trace>`) — replay
//! reconstructs the brain's commands bit-for-bit and verifies them against the
//! recorded log (ROADMAP Phase 3 exit criterion), with a `--step` inspector that
//! walks the session one event at a time.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use baton_core::{Command, Event, ModelSelector};
use baton_host::capabilities::{FsRead, FsWrite, Http, Shell};
use baton_host::policy::{AllowAll, Interactive};
use baton_host::{Engine, Inspector, Policy, StdoutFrontend, Trace};
use baton_providers::OpenAiAdapter;
use clap::{Parser, Subcommand};

const SYSTEM_PROMPT: &str = "\
You are Baton, a helpful coding agent running in a terminal. You can run shell \
commands, read and write files, and make HTTP requests via the provided tools. \
Prefer concrete actions over long explanations. When a task is complete, give a \
short summary.";

#[derive(Parser)]
#[command(
    name = "baton",
    version,
    about = "A portable, runtime-free agent harness"
)]
struct Cli {
    /// One-shot prompt. If omitted (and no subcommand), starts an interactive
    /// session.
    prompt: Vec<String>,

    /// Approve every tool call without prompting (the allow-all mode).
    #[arg(short = 'y', long = "yes")]
    yes: bool,

    /// Override the default model id (defaults to the `OPENAI_MODEL` env var,
    /// then the built-in `google/gemma-4-31B-it:together`).
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// Show tool results in full instead of collapsing large output to a head
    /// plus a "… +N lines" summary. Also enabled by `BATON_FULL_OUTPUT=1`.
    #[arg(long = "full-output")]
    full_output: bool,

    /// Record this session to a trace file (the ordered event stream + the
    /// durable log), replayable later with `baton replay <path>`.
    #[arg(long = "record", value_name = "PATH")]
    record: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Reconstruct a recorded session from a trace and verify it replays
    /// bit-for-bit (the Phase 3 exit criterion).
    Replay {
        /// Path to a `.trace.json` file produced by `--record`.
        trace: PathBuf,

        /// Step through the session one event at a time, printing the command(s)
        /// and log entry(ies) each event produced.
        #[arg(long = "step")]
        step: bool,
    },

    /// Resume a recorded session from a trace and continue it with a new turn.
    /// The brain is rebuilt from the trace's events (with no IO — recorded work
    /// is not re-run), the policy is restored from the trace, and the continued
    /// session keeps recording so it can be saved again (the Phase 3 P3-4 goal).
    Resume {
        /// Path to a `.trace.json` file produced by `--record` (or a prior
        /// `resume`). The continued session is written back here by default.
        trace: PathBuf,

        /// The new user turn to add. If omitted, starts an interactive session
        /// continuing from the trace.
        prompt: Vec<String>,

        /// Approve every tool call without prompting (the allow-all mode).
        #[arg(short = 'y', long = "yes")]
        yes: bool,

        /// Override the default model id used for the new turn(s).
        #[arg(short = 'm', long = "model")]
        model: Option<String>,

        /// Write the extended session to a different trace file instead of back
        /// to `<trace>` (so the original recording is left untouched).
        #[arg(long = "record", value_name = "PATH")]
        record: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    match cli.command.take() {
        Some(Cmd::Replay { trace, step }) => return run_replay(&trace, step),
        Some(Cmd::Resume {
            trace,
            prompt,
            yes,
            model,
            record,
        }) => return run_resume(trace, prompt, yes, model, record).await,
        None => {}
    }

    run_session(cli).await
}

/// Drive a live agent session (one-shot or interactive), optionally recording it.
async fn run_session(cli: Cli) -> Result<()> {
    let policy: Arc<dyn Policy> = if cli.yes {
        Arc::new(AllowAll)
    } else {
        Arc::new(Interactive)
    };

    let mut adapter = OpenAiAdapter::from_env()?;
    if let Some(model) = cli.model {
        adapter = adapter.with_model(model);
    }
    let mode = if cli.yes {
        "auto-approve"
    } else {
        "interactive"
    };
    let recording = cli.record.is_some();
    let banner = format!(
        "baton · model {} · {} · {mode}{}",
        adapter.model(),
        adapter.base_url(),
        if recording { " · recording" } else { "" },
    );
    // Dim the banner only on a real terminal (and not under NO_COLOR).
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[2m{banner}\x1b[0m");
    } else {
        eprintln!("{banner}");
    }

    // The `--full-output` flag forces full tool-result rendering; otherwise the
    // frontend honours `BATON_FULL_OUTPUT` on its own.
    let frontend = StdoutFrontend::new();
    let frontend = if cli.full_output {
        frontend.with_full_output(true)
    } else {
        frontend
    };

    // --- the "CLI on a laptop" host: ~10 lines on top of baton-host ----------
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), Arc::new(adapter))
        .capability(Arc::new(Shell))
        .capability(Arc::new(FsRead))
        .capability(Arc::new(FsWrite))
        .capability(Arc::new(Http::new()))
        .system_prompt(SYSTEM_PROMPT)
        .policy(policy)
        .record(recording)
        .frontend(Box::new(frontend))
        .build();
    // -------------------------------------------------------------------------

    if !cli.prompt.is_empty() {
        engine.user_turn(cli.prompt.join(" ")).await;
        engine.session_end();
        save_recording(&engine, cli.record.as_deref())?;
        return Ok(());
    }

    println!("baton — interactive session (Ctrl-D to exit)");
    loop {
        print!("\n› ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            println!();
            engine.session_end();
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        engine.user_turn(line.to_string()).await;
    }

    save_recording(&engine, cli.record.as_deref())?;
    Ok(())
}

/// `baton resume <trace> [prompt...]` — load a trace, rebuild the brain from its
/// recorded events (no IO: the recorded model/shell/http work is *not* re-run),
/// then continue the session with a new turn. The continued session keeps
/// recording and is saved back to `<trace>` by default (or to `--record <path>`),
/// so it grows into a trace that still replays bit-for-bit (Phase 3 P3-4).
async fn run_resume(
    trace_path: PathBuf,
    prompt: Vec<String>,
    yes: bool,
    model: Option<String>,
    record: Option<PathBuf>,
) -> Result<()> {
    let trace = Trace::load(&trace_path)
        .with_context(|| format!("loading trace {}", trace_path.display()))?;
    // Default: write the grown session back to the same file (so it accumulates).
    let out_path = record.unwrap_or_else(|| trace_path.clone());

    let policy: Arc<dyn Policy> = if yes {
        Arc::new(AllowAll)
    } else {
        Arc::new(Interactive)
    };

    let mut adapter = OpenAiAdapter::from_env()?;
    if let Some(model) = model {
        adapter = adapter.with_model(model);
    }
    let mode = if yes { "auto-approve" } else { "interactive" };
    let banner = format!(
        "baton · resuming {} ({} events) · model {} · {} · {mode} · recording → {}",
        trace_path.display(),
        trace.events.len(),
        adapter.model(),
        adapter.base_url(),
        out_path.display(),
    );
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[2m{banner}\x1b[0m");
    } else {
        eprintln!("{banner}");
    }

    // Resume rebuilds the brain from the trace (with zero IO) and restores the
    // recorded policy; `.resume` implies recording so the grown session re-saves.
    let mut engine = Engine::builder()
        .model(ModelSelector::named("big"), Arc::new(adapter))
        .capability(Arc::new(Shell))
        .capability(Arc::new(FsRead))
        .capability(Arc::new(FsWrite))
        .capability(Arc::new(Http::new()))
        .system_prompt(SYSTEM_PROMPT)
        .policy(policy)
        .resume(trace)
        .build();

    if !prompt.is_empty() {
        engine.user_turn(prompt.join(" ")).await;
        engine.session_end();
        save_recording(&engine, Some(out_path.as_path()))?;
        return Ok(());
    }

    println!("baton — resumed interactive session (Ctrl-D to exit)");
    loop {
        print!("\n› ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            println!();
            engine.session_end();
            break; // EOF
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        engine.user_turn(line.to_string()).await;
    }

    save_recording(&engine, Some(out_path.as_path()))?;
    Ok(())
}

/// Persist the recorded session, if `--record` was given.
fn save_recording(engine: &Engine, path: Option<&std::path::Path>) -> Result<()> {
    if let Some(path) = path {
        engine
            .save_trace(path)
            .with_context(|| format!("saving trace to {}", path.display()))?;
        eprintln!("recorded session → {}", path.display());
    }
    Ok(())
}

/// `baton replay <trace>` — load a trace, reconstruct the session through a
/// fresh brain, and verify the reconstructed log matches bit-for-bit. With
/// `--step`, walk it one event at a time first.
fn run_replay(path: &std::path::Path, step: bool) -> Result<()> {
    let trace = Trace::load(path).with_context(|| format!("loading trace {}", path.display()))?;
    eprintln!(
        "replaying {} · {} events · {} log entries · format v{}",
        path.display(),
        trace.events.len(),
        trace.log.len(),
        trace.meta.format_version,
    );

    if step {
        print_steps(&trace);
    }

    // The Phase 3 exit criterion: reconstruct commands + log bit-for-bit.
    let replay = baton_host::baton_replay::verify(&trace)
        .context("replay did not reconstruct the recorded log bit-for-bit")?;

    eprintln!(
        "✓ replay reconstructed {} commands; log matches the recording bit-for-bit",
        replay.commands.len(),
    );
    Ok(())
}

/// Step through a trace, printing each event with the commands and log entries
/// it produced. Host-side inspector; no IO of its own beyond stdout.
fn print_steps(trace: &Trace) {
    let mut inspector = Inspector::new(trace);
    let total = inspector.len();
    while let Some(step) = inspector.step() {
        println!("\n── step {}/{} ─────────────", step.index + 1, total);
        println!("  event:   {}", summarize_event(&step.event));
        if step.commands.is_empty() {
            println!("  command: (none)");
        } else {
            for cmd in &step.commands {
                println!("  command: {}", summarize_command(cmd));
            }
        }
        for entry in &step.appended {
            println!("  log[{}]: {:?}", entry.seq.0, entry.record);
        }
    }
}

fn summarize_event(event: &Event) -> String {
    use Event::*;
    match event {
        Tick { now } => format!("Tick(now={})", now.0),
        UserInput { content, mode } => format!("UserInput({content} · {mode:?})"),
        UserAbort => "UserAbort".to_string(),
        ModelDelta { op, .. } => format!("ModelDelta(op={})", op.0),
        ModelDone { op, .. } => format!("ModelDone(op={})", op.0),
        ModelError { op, .. } => format!("ModelError(op={})", op.0),
        CapabilityChunk { op, .. } => format!("CapabilityChunk(op={})", op.0),
        CapabilityDone { op, .. } => format!("CapabilityDone(op={})", op.0),
        CapabilityError { op, .. } => format!("CapabilityError(op={})", op.0),
        PermissionDecision { op, decision } => {
            format!("PermissionDecision(op={} · {decision:?})", op.0)
        }
        OpCancelled { op } => format!("OpCancelled(op={})", op.0),
        other => format!("{other:?}"),
    }
}

fn summarize_command(cmd: &Command) -> String {
    match cmd {
        Command::StartModelCall { op, model, .. } => {
            format!("StartModelCall(op={} · {model:?})", op.0)
        }
        Command::StartCapability { op, name, .. } => {
            format!("StartCapability(op={} · {name})", op.0)
        }
        Command::RequestPermission { op, request } => {
            format!("RequestPermission(op={} · {})", op.0, request.capability)
        }
        Command::Cancel { op } => format!("Cancel(op={})", op.0),
        Command::Emit(_) => "Emit(…)".to_string(),
        Command::Checkpoint => "Checkpoint".to_string(),
        Command::Done { reason } => format!("Done({reason:?})"),
        other => format!("{other:?}"),
    }
}
