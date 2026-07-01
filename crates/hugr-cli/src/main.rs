//! `hugr` — the batteries-included showcase CLI.
//!
//! The engine setup below is the "CLI on a laptop" host: ~10 lines on top of
//! `hugr-host` (ROADMAP Phase 1 exit criterion).
//!
//! Beyond running sessions, the CLI can **record** a session to a trace
//! (`--record <path>`) and **replay** one (`hugr replay <trace>`) — replay
//! reconstructs the brain's commands bit-for-bit and verifies them against the
//! recorded log (ROADMAP Phase 3 exit criterion), with a `--step` inspector that
//! walks the session one event at a time.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use hugr_core::{
    AgentSeed, Command, ContextDisposition, ContextPlan, ContextSource, Event, ModelSelector,
    Record, SkillDescriptor, ToolSchema,
};
use hugr_host::capabilities::{FsRead, FsWrite, Http, Shell};
use hugr_host::policy::{AllowAll, AutoApprove};
use hugr_host::{
    Capability, CheckpointCadence, CronExpr, Engine, EngineBuilder, Inspector, McpServerConfig,
    Policy, Schedule, SkillBundle, SpendReport, StdoutFrontend, Trace, TriggerTarget, spend_report,
};
use hugr_providers::TierModelConfigSet;
use serde::Deserialize;

const SYSTEM_PROMPT: &str = "\
You are Hugr, a helpful coding agent running in a terminal. You can run shell \
commands, read and write files, and make HTTP requests via the provided tools. \
Prefer concrete actions over long explanations. When a task is complete, give a \
short summary.";

#[derive(Parser)]
#[command(
    name = "hugr",
    version,
    about = "A portable, runtime-free agent harness"
)]
struct Cli {
    /// One-shot prompt. If omitted (and no subcommand), starts an interactive
    /// session.
    prompt: Vec<String>,

    /// Allow every gated tool call without running the auto-approve judge.
    #[arg(short = 'y', long = "yolo", visible_alias = "yes")]
    yolo: bool,

    /// Override the model id for all tiers (defaults to the `HUGR_MODEL_*`
    /// env vars, then the built-in HF router model).
    #[arg(short = 'm', long = "model")]
    model: Option<String>,

    /// Show tool results in full instead of collapsing large output to a head
    /// plus a "… +N lines" summary. Also enabled by `HUGR_FULL_OUTPUT=1`.
    #[arg(long = "full-output")]
    full_output: bool,

    /// Record this session to a trace file (the ordered event stream + the
    /// durable log), replayable later with `hugr replay <path>`.
    #[arg(long = "record", value_name = "PATH")]
    record: Option<PathBuf>,

    /// Load a plugin: a program (optionally with args) that speaks the Hugr
    /// plugin protocol over stdio. Repeatable. Each plugin's tools are
    /// registered as ordinary capabilities. E.g. `--plugin ./my-plugin`.
    #[arg(long = "plugin", value_name = "CMD")]
    plugins: Vec<String>,

    /// Load an MCP stdio server from a command spec. Repeatable. Tools are
    /// registered as ordinary capabilities named `mcp__<server>__<tool>`.
    #[arg(long = "mcp", value_name = "CMD")]
    mcp: Vec<String>,

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

        /// Allow every gated tool call without running the auto-approve judge.
        #[arg(short = 'y', long = "yolo", visible_alias = "yes")]
        yolo: bool,

        /// Override the default model id used for the new turn(s).
        #[arg(short = 'm', long = "model")]
        model: Option<String>,

        /// Write the extended session to a different trace file instead of back
        /// to `<trace>` (so the original recording is left untouched).
        #[arg(long = "record", value_name = "PATH")]
        record: Option<PathBuf>,
    },

    /// Fire a prompt on a host-side cron cadence. Each fire injects the prompt
    /// into a resumed trace, a named persistent session, or a fresh trace.
    Schedule {
        /// Cron cadence. Supported: `@every 10s`, `@every 5m`, `* * * * *`,
        /// `*/N * * * *`.
        #[arg(long = "cron", default_value = "@every 1h")]
        cron: String,

        /// Run one fire and exit instead of sleeping forever.
        #[arg(long = "once")]
        once: bool,

        /// Resume this existing trace on every fire.
        #[arg(long = "trace", value_name = "PATH")]
        trace: Option<PathBuf>,

        /// Target a named persistent session under `--sessions-dir`.
        #[arg(long = "session", value_name = "NAME")]
        session: Option<String>,

        /// Directory for named persistent sessions.
        #[arg(
            long = "sessions-dir",
            value_name = "DIR",
            default_value = ".hugr/sessions"
        )]
        sessions_dir: PathBuf,

        /// Start a fresh session per fire and write it to this trace path.
        #[arg(long = "fresh", value_name = "PATH")]
        fresh: Option<PathBuf>,

        /// Allow every gated tool call without running the auto-approve judge.
        #[arg(short = 'y', long = "yolo", visible_alias = "yes")]
        yolo: bool,

        /// Override the default model id used for scheduled fires.
        #[arg(short = 'm', long = "model")]
        model: Option<String>,

        /// Prompt to inject on each scheduled fire.
        prompt: Vec<String>,
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
            yolo,
            model,
            record,
        }) => return run_resume(trace, prompt, yolo, model, record).await,
        Some(Cmd::Schedule {
            cron,
            once,
            trace,
            session,
            sessions_dir,
            fresh,
            yolo,
            model,
            prompt,
        }) => {
            return run_schedule(ScheduleArgs {
                cron,
                once,
                trace,
                session,
                sessions_dir,
                fresh,
                yolo,
                model,
                prompt,
            })
            .await;
        }
        None => {}
    }

    run_session(cli).await
}

/// Drive a live agent session (one-shot or interactive), optionally recording it.
async fn run_session(cli: Cli) -> Result<()> {
    let models = build_model_config(cli.model)?;
    let policy = select_policy(cli.yolo, &models)?;
    let mapping = models.mapping_summary();
    let base_url = models.base_url.clone();
    let recording = cli.record.is_some();
    let mode = if cli.yolo { "yolo" } else { "auto-approve" };
    let skills = hugr_host::skills::discover().context("discovering skill bundles")?;
    print_banner(&format!(
        "hugr · model {} · {} · {mode} · {} skills{}",
        mapping,
        base_url,
        skills.len(),
        if recording { " · recording" } else { "" },
    ));

    // The `--full-output` flag forces full tool-result rendering; otherwise the
    // frontend honours `HUGR_FULL_OUTPUT` on its own.
    let frontend = StdoutFrontend::new();
    let frontend = if cli.full_output {
        frontend.with_full_output(true)
    } else {
        frontend
    };

    // --- the "CLI on a laptop" host: ~10 lines on top of hugr-host ----------
    let mut builder = base_builder(models, policy)?
        .skills(skill_descriptors(&skills))
        .record(recording)
        .frontend(Box::new(frontend));
    if let Some(path) = cli.record.clone() {
        builder = builder.checkpoint(path, CheckpointCadence::EveryEvent);
    }
    // Load any --plugin programs and register their tools as capabilities.
    for spec in &cli.plugins {
        let mut parts = spec.split_whitespace();
        let program = parts.next().unwrap_or_default();
        let args: Vec<&str> = parts.collect();
        let caps = hugr_host::plugins::load_subprocess(program, args)
            .await
            .with_context(|| format!("loading plugin `{spec}`"))?;
        for cap in caps {
            builder = builder.capability(cap);
        }
    }
    let (mcp_caps, mcp_status) = load_mcp_servers(&cli.mcp).await?;
    for cap in mcp_caps {
        builder = builder.capability(cap);
    }
    let mut engine = builder.build();
    // -------------------------------------------------------------------------

    drive_session(
        &mut engine,
        cli.prompt,
        "hugr — interactive session (Ctrl-D to exit)",
        &mapping,
        cli.record.as_deref(),
        &HostStatus {
            mcp_servers: mcp_status,
            skills: skill_status(&skills),
        },
    )
    .await
}

/// `hugr resume <trace> [prompt...]` — load a trace, rebuild the brain from its
/// recorded events (no IO: the recorded model/shell/http work is *not* re-run),
/// then continue the session with a new turn. The continued session keeps
/// recording and is saved back to `<trace>` by default (or to `--record <path>`),
/// so it grows into a trace that still replays bit-for-bit (Phase 3 P3-4).
async fn run_resume(
    trace_path: PathBuf,
    prompt: Vec<String>,
    yolo: bool,
    model: Option<String>,
    record: Option<PathBuf>,
) -> Result<()> {
    let trace = Trace::load(&trace_path)
        .with_context(|| format!("loading trace {}", trace_path.display()))?;
    // Default: write the grown session back to the same file (so it accumulates).
    let out_path = record.unwrap_or_else(|| trace_path.clone());

    let models = build_model_config(model)?;
    let policy = select_policy(yolo, &models)?;
    let mapping = models.mapping_summary();
    let base_url = models.base_url.clone();
    let mode = if yolo { "yolo" } else { "auto-approve" };
    print_banner(&format!(
        "hugr · resuming {} ({} events) · model {} · {} · {mode} · recording → {}",
        trace_path.display(),
        trace.events.len(),
        mapping,
        base_url,
        out_path.display(),
    ));

    // Resume rebuilds the brain from the trace (with zero IO) and restores the
    // recorded policy; `.resume` implies recording so the grown session re-saves.
    let mut engine = base_builder(models, policy)?
        .resume(trace)
        .checkpoint(out_path.clone(), CheckpointCadence::EveryEvent)
        .build();
    let host_status = HostStatus::default();

    drive_session(
        &mut engine,
        prompt,
        "hugr — resumed interactive session (Ctrl-D to exit)",
        &mapping,
        Some(out_path.as_path()),
        &host_status,
    )
    .await
}

struct ScheduleArgs {
    cron: String,
    once: bool,
    trace: Option<PathBuf>,
    session: Option<String>,
    sessions_dir: PathBuf,
    fresh: Option<PathBuf>,
    yolo: bool,
    model: Option<String>,
    prompt: Vec<String>,
}

async fn run_schedule(args: ScheduleArgs) -> Result<()> {
    let prompt = args.prompt.join(" ");
    anyhow::ensure!(
        !prompt.trim().is_empty(),
        "scheduled prompt cannot be empty"
    );
    let cron = CronExpr::parse(&args.cron)?;
    let target = schedule_target(args.trace, args.session, args.sessions_dir, args.fresh)?;
    let schedule = Schedule::new(cron, target, prompt);
    let mode = if args.yolo { "yolo" } else { "auto-approve" };

    loop {
        let models = build_model_config(args.model.clone())?;
        let policy = select_policy(args.yolo, &models)?;
        let mapping = models.mapping_summary();
        let base_url = models.base_url.clone();
        print_banner(&format!(
            "hugr · scheduled fire {} · model {} · {} · {mode}",
            schedule.cron.source(),
            mapping,
            base_url,
        ));
        let path = hugr_host::fire_once(base_builder(models, policy.clone())?, &schedule).await?;
        eprintln!("scheduled fire recorded → {}", path.display());

        if args.once {
            break;
        }
        tokio::time::sleep(schedule.cron.interval()).await;
    }
    Ok(())
}

fn schedule_target(
    trace: Option<PathBuf>,
    session: Option<String>,
    sessions_dir: PathBuf,
    fresh: Option<PathBuf>,
) -> Result<TriggerTarget> {
    let selected = trace.is_some() as u8 + session.is_some() as u8 + fresh.is_some() as u8;
    anyhow::ensure!(
        selected == 1,
        "choose exactly one schedule target: --trace, --session, or --fresh"
    );
    if let Some(trace) = trace {
        Ok(TriggerTarget::ResumeExisting { trace })
    } else if let Some(name) = session {
        Ok(TriggerTarget::NamedPersistent {
            dir: sessions_dir,
            name,
        })
    } else {
        Ok(TriggerTarget::FreshSession {
            trace: fresh.expect("selected checked above"),
        })
    }
}

/// The host permission policy for the chosen approval mode (`--yolo` = allow-all).
fn select_policy(yolo: bool, models: &TierModelConfigSet) -> Result<Arc<dyn Policy>> {
    if yolo {
        Ok(Arc::new(AllowAll))
    } else {
        let judge = models
            .adapters_from_env()?
            .into_iter()
            .find(|(selector, _)| *selector == ModelSelector::named("small"))
            .map(|(_, adapter)| adapter)
            .context("model tier config did not include a `small` judge tier")?;
        Ok(Arc::new(AutoApprove::new(Arc::new(judge))))
    }
}

/// Build the model tier config from `HUGR_CONFIG` / `HUGR_MODEL_*` /
/// `HUGR_BASE_URL`, then apply a `--model` override to all three tiers.
fn build_model_config(model: Option<String>) -> Result<TierModelConfigSet> {
    let mut models = TierModelConfigSet::from_env()?;
    if let Some(model) = model {
        models = models.with_all_models(model);
    }
    Ok(models)
}

#[derive(Clone, Debug, Default)]
struct HostStatus {
    mcp_servers: Vec<McpServerStatus>,
    skills: Vec<SkillStatus>,
}

#[derive(Clone, Debug)]
struct McpServerStatus {
    name: String,
    tools: Vec<String>,
}

#[derive(Clone, Debug)]
struct SkillStatus {
    id: String,
    title: String,
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum McpConfigEntry {
    Spec(String),
    Object {
        name: Option<String>,
        #[serde(alias = "cmd")]
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

async fn load_mcp_servers(
    cli_specs: &[String],
) -> Result<(Vec<Arc<dyn Capability>>, Vec<McpServerStatus>)> {
    let mut configs = read_mcp_config()?;
    for (index, spec) in cli_specs.iter().enumerate() {
        configs.push(mcp_config_from_spec(
            format!("cli{}", index + 1),
            spec,
            Vec::new(),
        )?);
    }

    let mut capabilities = Vec::new();
    let mut status = Vec::new();
    for config in configs {
        let server_name = config.name.clone();
        let caps = hugr_host::mcp::load_stdio(config)
            .await
            .with_context(|| format!("loading MCP server `{server_name}`"))?;
        let tools = caps.iter().map(|cap| cap.name().to_string()).collect();
        status.push(McpServerStatus {
            name: server_name,
            tools,
        });
        capabilities.extend(caps);
    }
    Ok((capabilities, status))
}

fn read_mcp_config() -> Result<Vec<McpServerConfig>> {
    let Some(path) = std::env::var_os("HUGR_CONFIG") else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading Hugr config from {}",
            PathBuf::from(&path).display()
        )
    })?;
    let root: serde_json::Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "parsing Hugr config from {}",
            PathBuf::from(&path).display()
        )
    })?;
    let Some(raw) = root.get("mcp").or_else(|| root.get("mcp_servers")) else {
        return Ok(Vec::new());
    };
    parse_mcp_entries(raw.clone())
}

fn parse_mcp_entries(raw: serde_json::Value) -> Result<Vec<McpServerConfig>> {
    let entries: Vec<McpConfigEntry> = serde_json::from_value(raw)
        .context("parsing `mcp` section; expected strings or { name, command, args } objects")?;
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| match entry {
            McpConfigEntry::Spec(spec) => {
                mcp_config_from_spec(format!("config{}", index + 1), &spec, Vec::new())
            }
            McpConfigEntry::Object {
                name,
                command,
                args,
            } => mcp_config_from_spec(
                name.unwrap_or_else(|| format!("config{}", index + 1)),
                &command,
                args,
            ),
        })
        .collect()
}

fn mcp_config_from_spec(
    name: impl Into<String>,
    spec: &str,
    extra_args: Vec<String>,
) -> Result<McpServerConfig> {
    let name = name.into();
    let mut parts = spec.split_whitespace();
    let program = parts
        .next()
        .with_context(|| format!("empty MCP command spec for `{name}`"))?;
    let mut config = McpServerConfig::new(name, program);
    for arg in parts {
        config = config.arg(arg);
    }
    for arg in extra_args {
        config = config.arg(arg);
    }
    Ok(config)
}

fn skill_descriptors(skills: &[SkillBundle]) -> Vec<SkillDescriptor> {
    skills
        .iter()
        .map(|skill| {
            let mut descriptor = SkillDescriptor::new(&skill.id, &skill.title, &skill.instructions)
                .with_est_tokens(estimate_text_tokens(&skill.instructions));
            if let Some(summary) = &skill.summary {
                descriptor = descriptor.with_summary(summary.clone());
            }
            descriptor
        })
        .collect()
}

fn skill_status(skills: &[SkillBundle]) -> Vec<SkillStatus> {
    skills
        .iter()
        .map(|skill| SkillStatus {
            id: skill.id.clone(),
            title: skill.title.clone(),
            summary: skill.summary.clone(),
        })
        .collect()
}

fn estimate_text_tokens(text: &str) -> u32 {
    let bytes = text.len() as u64;
    bytes.div_ceil(4).max(1).min(u32::MAX as u64) as u32
}

/// Print a startup banner to stderr, dimmed only on a real terminal (and not
/// under `NO_COLOR`).
fn print_banner(text: &str) {
    if std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        eprintln!("\x1b[2m{text}\x1b[0m");
    } else {
        eprintln!("{text}");
    }
}

/// The shared "CLI on a laptop" host: register the model + capabilities and the
/// permission policy. Callers add `.record()`/`.resume()`/`.frontend()` and
/// `.build()`. Keeping this in one place is what keeps the host setup ~10 lines.
fn base_builder(models: TierModelConfigSet, policy: Arc<dyn Policy>) -> Result<EngineBuilder> {
    let mut builder = Engine::builder().default_model(ModelSelector::named("medium"));
    for (selector, adapter) in models.adapters_from_env()? {
        builder = builder.model(selector, Arc::new(adapter));
    }
    Ok(builder
        .capability(Arc::new(Shell))
        .capability(Arc::new(FsRead))
        .capability(Arc::new(FsWrite))
        .capability(Arc::new(Http::new()))
        // A `task` sub-agent tool (Phase 6): the model can delegate a self-
        // contained unit of work to a child agent seeded with the full context.
        // The child reuses this host's tools (optionally narrowed via `tools`).
        .agent(task_agent_schema(), AgentSeed::ForkFull)
        .system_prompt(SYSTEM_PROMPT)
        .policy(policy))
}

/// The schema advertised for the built-in `task` sub-agent tool.
fn task_agent_schema() -> ToolSchema {
    ToolSchema::new(
        "task",
        "Delegate a self-contained sub-task to a child agent. It runs with its \
         own turn loop and the same tools, and returns a text digest. Use for \
         focused work you want handled end-to-end (e.g. 'find and summarize all \
         TODOs').",
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "The sub-task instruction." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Optional allowlist of tool names the sub-agent may use."
                }
            },
            "required": ["prompt"]
        }),
    )
}

/// Run the session: a one-shot turn if `prompt` is non-empty, otherwise an
/// interactive REPL (`intro` is its header). Saves the recording to `out_path`
/// (if any) at the end. Shared by the live and resumed paths.
async fn drive_session(
    engine: &mut Engine,
    prompt: Vec<String>,
    intro: &str,
    tier_mapping: &str,
    out_path: Option<&std::path::Path>,
    host_status: &HostStatus,
) -> Result<()> {
    if !prompt.is_empty() {
        let text = prompt.join(" ");
        if !handle_repl_command(engine, &text, tier_mapping, host_status).await? {
            engine.user_turn(text).await;
        }
        engine.session_end();
        return save_recording(engine, out_path);
    }

    println!("{intro}");
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
        if handle_repl_command(engine, line, tier_mapping, host_status).await? {
            continue;
        }
        engine.user_turn(line.to_string()).await;
    }

    save_recording(engine, out_path)
}

async fn handle_repl_command(
    engine: &mut Engine,
    line: &str,
    tier_mapping: &str,
    host_status: &HostStatus,
) -> Result<bool> {
    let mut parts = line.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(false);
    };
    match command {
        "/context" => {
            print_context_plan(&engine.context_plan());
            Ok(true)
        }
        "/compact" => {
            engine.compact_context().await;
            Ok(true)
        }
        "/model" => {
            print_model_status(engine, tier_mapping);
            Ok(true)
        }
        "/tier" => {
            handle_tier_command(engine, parts.next());
            Ok(true)
        }
        "/status" => {
            print_status(engine, tier_mapping, host_status);
            Ok(true)
        }
        "/skills" => {
            print_skills(engine, host_status);
            Ok(true)
        }
        _ if command.starts_with('/') => {
            eprintln!("unknown command: {line}");
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn handle_tier_command(engine: &mut Engine, tier: Option<&str>) {
    match tier {
        None => print_tier_override(engine),
        Some(name @ ("small" | "medium" | "big")) => {
            let selector = ModelSelector::named(name);
            engine.override_next_model(Some(selector.clone()));
            println!("next turn tier override: {}", selector_label(&selector));
        }
        Some("auto" | "clear") => {
            engine.override_next_model(None);
            println!("next turn tier override cleared");
        }
        Some(other) => {
            eprintln!("usage: /tier [small|medium|big|auto] (got {other})");
        }
    }
}

fn print_model_status(engine: &Engine, tier_mapping: &str) {
    println!("models: {tier_mapping}");
    print_tier_override(engine);
}

fn print_tier_override(engine: &Engine) {
    match engine.brain().state().next_model_override() {
        Some(selector) => println!("next turn tier override: {}", selector_label(selector)),
        None => println!("next turn tier override: auto"),
    }
}

fn print_status(engine: &Engine, tier_mapping: &str, host_status: &HostStatus) {
    let plan = engine.context_plan();
    let report = spend_report(engine.brain().state().log());
    println!("models: {tier_mapping}");
    print_tier_override(engine);
    println!(
        "context: {}/{} tokens ({:.0}% full)",
        plan.totals.used_tokens,
        plan.budget.max_tokens,
        context_percent(plan.totals.used_tokens, plan.budget.max_tokens)
    );
    print_mcp_status(host_status);
    print_active_skill(engine);
    print_spend_report(&report);
}

fn print_mcp_status(host_status: &HostStatus) {
    if host_status.mcp_servers.is_empty() {
        println!("mcp: no connected servers");
        return;
    }
    println!("mcp:");
    for server in &host_status.mcp_servers {
        if server.tools.is_empty() {
            println!("- {} · no tools", server.name);
        } else {
            println!("- {} · {}", server.name, server.tools.join(", "));
        }
    }
}

fn print_skills(engine: &Engine, host_status: &HostStatus) {
    if host_status.skills.is_empty() {
        println!("skills: none discovered");
        return;
    }
    let active = active_skill(engine);
    println!("skills:");
    for skill in &host_status.skills {
        let marker = if active.as_deref() == Some(skill.id.as_str()) {
            " · active"
        } else {
            ""
        };
        match &skill.summary {
            Some(summary) if !summary.is_empty() => {
                println!("- {} · {}{} — {}", skill.id, skill.title, marker, summary);
            }
            _ => println!("- {} · {}{}", skill.id, skill.title, marker),
        }
    }
}

fn print_active_skill(engine: &Engine) {
    match active_skill(engine) {
        Some(id) => println!("active skill: {id}"),
        None => println!("active skill: none"),
    }
}

fn active_skill(engine: &Engine) -> Option<String> {
    engine
        .brain()
        .state()
        .log()
        .iter()
        .rev()
        .find_map(|entry| match &entry.record {
            Record::SkillActivated { id, .. } => Some(id.clone()),
            _ => None,
        })
}

fn print_spend_report(report: &SpendReport) {
    if report.tiers.is_empty() {
        println!("spend: no model calls yet");
    } else {
        println!("spend:");
        for tier in &report.tiers {
            let cost = tier
                .cost
                .map(|cost| format!(" · ${cost:.6}"))
                .unwrap_or_default();
            println!(
                "- {} · {} calls · in {} / out {} tok · {} ms{}",
                selector_label(&tier.selector),
                tier.model_calls,
                tier.input_tokens,
                tier.output_tokens,
                tier.latency_ms,
                cost
            );
        }
    }
    if report.recent_routing.is_empty() {
        println!("routing: no recorded model routing yet");
    } else {
        println!("routing:");
        for decision in &report.recent_routing {
            println!(
                "- {} · {}",
                selector_label(&decision.selector),
                decision.reasons.join("; ")
            );
        }
    }
}

fn context_percent(used: u64, max: u64) -> f64 {
    if max == 0 {
        0.0
    } else {
        used as f64 * 100.0 / max as f64
    }
}

fn selector_label(selector: &ModelSelector) -> String {
    match selector {
        ModelSelector::Named(name) => name.clone(),
        other => format!("{other:?}"),
    }
}

fn print_context_plan(plan: &ContextPlan) {
    let totals = &plan.totals;
    println!(
        "context: {}/{} tokens used (included {}, summarized {}, referenced {}, omitted {})",
        totals.used_tokens,
        plan.budget.max_tokens,
        totals.included_tokens,
        totals.summarized_tokens,
        totals.referenced_tokens,
        totals.omitted_tokens
    );
    println!(
        "blocks: retained {} · summaries {} · refs {} · omitted {}",
        count_disposition(plan, "included"),
        count_disposition(plan, "summarized"),
        count_disposition(plan, "referenced"),
        count_disposition(plan, "omitted")
    );
    for entry in &plan.entries {
        println!(
            "- {} · {} · {} tok · {}",
            context_source_label(&entry.source),
            disposition_label(&entry.disposition),
            entry.est_tokens,
            entry.reason
        );
    }
}

fn count_disposition(plan: &ContextPlan, kind: &str) -> usize {
    plan.entries
        .iter()
        .filter(|entry| disposition_label(&entry.disposition) == kind)
        .count()
}

fn context_source_label(source: &ContextSource) -> String {
    match source {
        ContextSource::System => "system".to_string(),
        ContextSource::LogEntry { seq } => format!("log:{}", seq.0),
        ContextSource::Synthetic { label } => format!("synthetic:{label}"),
        other => format!("{other:?}"),
    }
}

fn disposition_label(disposition: &ContextDisposition) -> &'static str {
    match disposition {
        ContextDisposition::Included { .. } => "included",
        ContextDisposition::Referenced { .. } => "referenced",
        ContextDisposition::Summarized { .. } => "summarized",
        ContextDisposition::Omitted => "omitted",
        _ => "unknown",
    }
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

/// `hugr replay <trace>` — load a trace, reconstruct the session through a
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
    let replay = hugr_host::hugr_replay::verify(&trace)
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
        UserInput { content, mode, .. } => format!("UserInput({content} · {mode:?})"),
        UserAbort => "UserAbort".to_string(),
        CompactContext => "CompactContext".to_string(),
        ModelOverride { selector } => format!("ModelOverride({selector:?})"),
        ModelDelta { op, .. } => format!("ModelDelta(op={})", op.0),
        ModelDone { op, .. } => format!("ModelDone(op={})", op.0),
        ModelError { op, .. } => format!("ModelError(op={})", op.0),
        CapabilityChunk { op, .. } => format!("CapabilityChunk(op={})", op.0),
        CapabilityDone { op, .. } => format!("CapabilityDone(op={})", op.0),
        CapabilityError { op, .. } => format!("CapabilityError(op={})", op.0),
        AgentDone { op, .. } => format!("AgentDone(op={})", op.0),
        AgentError { op, .. } => format!("AgentError(op={})", op.0),
        PermissionDecision { op, decision, .. } => {
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
        Command::StartAgent { op, seed, .. } => {
            format!("StartAgent(op={} · seed {} entries)", op.0, seed.len())
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsString;

    #[test]
    fn mcp_command_spec_splits_program_and_args() {
        let config = mcp_config_from_spec("cli1", "server --root .", vec!["--extra".into()])
            .expect("valid command spec");
        assert_eq!(config.name, "cli1");
        assert_eq!(config.program, OsString::from("server"));
        assert_eq!(
            config.args,
            vec![
                OsString::from("--root"),
                OsString::from("."),
                OsString::from("--extra")
            ]
        );
    }

    #[test]
    fn mcp_config_accepts_strings_and_named_objects() {
        let configs = parse_mcp_entries(json!([
            "python3 -m first",
            { "name": "fs", "command": "mcp-filesystem", "args": ["."] },
            { "name": "git", "cmd": "mcp-git --stdio" }
        ]))
        .expect("valid mcp config");

        assert_eq!(configs.len(), 3);
        assert_eq!(configs[0].name, "config1");
        assert_eq!(configs[0].program, OsString::from("python3"));
        assert_eq!(
            configs[0].args,
            vec![OsString::from("-m"), OsString::from("first")]
        );
        assert_eq!(configs[1].name, "fs");
        assert_eq!(configs[1].args, vec![OsString::from(".")]);
        assert_eq!(configs[2].name, "git");
        assert_eq!(configs[2].args, vec![OsString::from("--stdio")]);
    }
}
