use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use hugr_core::ModelSelector;
use hugr_docs::{DocsConfig, DocsRoot, JsonFrontend, SYSTEM_PROMPT, build_answer, user_prompt};
use hugr_host::{Engine, policy::AllowAll};
use hugr_providers::OpenAiAdapter;

#[derive(Parser)]
#[command(
    name = "hugr-docs",
    version,
    about = "Answer questions from a read-only docs folder as JSON"
)]
struct Cli {
    /// Folder containing the documentation archive.
    docs_path: PathBuf,

    /// Question to answer from the documentation.
    question: Vec<String>,

    /// Override the model id. Defaults to HUGR_DOCS_MODEL, then google/gemma-4-31B-it:cerebras.
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// Pretty-print JSON output.
    #[arg(long = "pretty")]
    pretty: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let question = cli.question.join(" ");
    anyhow::ensure!(!question.trim().is_empty(), "question cannot be empty");

    let config = DocsConfig::from_env(cli.docs_path.clone(), cli.model.clone())?;
    let docs = DocsRoot::new(&config.root)?;
    eprintln!(
        "[hugr-docs] start docs_path={} model={} endpoint={}",
        config.root.display(),
        config.model,
        config.base_url
    );
    let selector = ModelSelector::named("docs");
    let adapter = OpenAiAdapter::new(config.api_key.clone(), config.model.clone())
        .with_base_url(config.base_url.clone())
        .with_default_params(config.sampling.clone());

    let mut builder = Engine::builder()
        .model(selector.clone(), Arc::new(adapter))
        .default_model(selector)
        .system_prompt(SYSTEM_PROMPT)
        .sampling(config.sampling.clone())
        .policy(Arc::new(AllowAll))
        .frontend(Box::new(JsonFrontend));
    for capability in docs.capabilities() {
        builder = builder.capability(capability);
    }

    let mut engine = builder.build();
    let started = Instant::now();
    eprintln!("[hugr-docs] question={}", serde_json::to_string(&question)?);
    engine.user_turn(user_prompt(&question)).await;
    engine.session_end();

    let answer = build_answer(engine.brain().state().log(), &config, started.elapsed())
        .context("building JSON answer from Hugr session")?;
    eprintln!("[hugr-docs] writing_json_stdout");
    if cli.pretty {
        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        println!("{}", serde_json::to_string(&answer)?);
    }
    Ok(())
}
