use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use hugr_docs::{DocsConfigOptions, answer_with_options};

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

    /// Resume/fork from a previously returned trace id.
    #[arg(long = "trace")]
    trace_id: Option<String>,

    /// Directory for persisted traces. Defaults to HUGR_DOCS_TRACE_DIR, then .hugr-docs-traces.
    #[arg(long = "trace-dir")]
    trace_dir: Option<PathBuf>,

    /// Pretty-print JSON output.
    #[arg(long = "pretty")]
    pretty: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let question = cli.question.join(" ");
    let mut options = DocsConfigOptions::new();
    if let Some(model) = cli.model.clone() {
        options = options.with_model(model);
    }
    if let Some(trace_id) = cli.trace_id.clone() {
        options = options.with_trace_id(trace_id);
    }
    if let Some(trace_dir) = cli.trace_dir.clone() {
        options = options.with_trace_dir(trace_dir);
    }
    // `answer_with_options` swallows every failure (bad API key, missing docs
    // root, model produced no final answer, …) into a `"status": "error"` JSON
    // object so stdout always carries a parseable result and exit stays 0.
    let answer = answer_with_options(cli.docs_path, options, &question).await?;
    if cli.pretty {
        println!("{}", serde_json::to_string_pretty(&answer)?);
    } else {
        println!("{}", serde_json::to_string(&answer)?);
    }
    Ok(())
}
