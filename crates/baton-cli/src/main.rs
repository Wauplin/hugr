//! `baton` — the batteries-included showcase CLI.
//!
//! The engine setup below is the "CLI on a laptop" host: ~10 lines on top of
//! `baton-host` (ROADMAP Phase 1 exit criterion).

use std::io::{IsTerminal, Write};
use std::sync::Arc;

use anyhow::Result;
use baton_core::ModelSelector;
use baton_host::capabilities::{FsRead, FsWrite, Http, Shell};
use baton_host::policy::{AllowAll, Interactive};
use baton_host::{Engine, Policy, StdoutFrontend};
use baton_providers::OpenAiAdapter;
use clap::Parser;

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
    /// One-shot prompt. If omitted, starts an interactive session.
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
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
    let banner = format!(
        "baton · model {} · {} · {mode}",
        adapter.model(),
        adapter.base_url(),
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
        .frontend(Box::new(frontend))
        .build();
    // -------------------------------------------------------------------------

    if !cli.prompt.is_empty() {
        engine.user_turn(cli.prompt.join(" ")).await;
        engine.session_end();
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

    Ok(())
}
