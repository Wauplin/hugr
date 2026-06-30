//! `baton` — the batteries-included showcase CLI.
//!
//! The engine setup below is the "CLI on a laptop" host: ~10 lines on top of
//! `baton-host` (ROADMAP Phase 1 exit criterion).

use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use baton_core::ModelSelector;
use baton_host::capabilities::{FsRead, FsWrite, Http, Shell};
use baton_host::policy::{AllowAll, Interactive};
use baton_host::{Engine, Policy};
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let policy: Arc<dyn Policy> = if cli.yes {
        Arc::new(AllowAll)
    } else {
        Arc::new(Interactive)
    };

    // --- the "CLI on a laptop" host: ~10 lines on top of baton-host ----------
    let mut engine = Engine::builder()
        .model(
            ModelSelector::named("big"),
            Arc::new(OpenAiAdapter::from_env()?),
        )
        .capability(Arc::new(Shell))
        .capability(Arc::new(FsRead))
        .capability(Arc::new(FsWrite))
        .capability(Arc::new(Http::new()))
        .system_prompt(SYSTEM_PROMPT)
        .policy(policy)
        .build();
    // -------------------------------------------------------------------------

    if !cli.prompt.is_empty() {
        engine.user_turn(cli.prompt.join(" ")).await;
        return Ok(());
    }

    println!("baton — interactive session (Ctrl-D to exit)");
    loop {
        print!("\n› ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            println!();
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
