//! The universal CLI surface every built agent binary wraps (ROADMAP T2.1,
//! ARCHITECTURE §21.1).
//!
//! A binary produced by `hugr build` embeds its agent bundle as a
//! [`bundle`] and calls [`run_cli`] from `main`. Every built agent has the same
//! shape:
//!
//! ```text
//! <agent> "question" [--trace <id>] [--json|--pretty] [--blob <path>...]
//! <agent> --describe | --config | --traces
//! ```
//!
//! One JSON [`Answer`] on stdout, diagnostics on stderr, and **exit 0** for the
//! ask path — a bad manifest, a build failure, or an infra `AskError` all come
//! back as `status: "error"` answers (the `hugr-docs` contract, now universal).
//! The audit sub-commands (`--describe`/`--config`/`--traces`) are inspection
//! surfaces: they print JSON and exit non-zero on failure.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::{Arg, ArgAction, Command};
use hugr_agent::{Agent, Answer, AnswerMeta, Ask, BlobHandle, BlobRef, STATUS_ERROR, TraceId};
use serde_json::json;

use crate::bundle;
use crate::manifest::AgentDefinition;
use crate::runtime::{RuntimeOptions, build_agent_with_options};
use crate::runtime_args::{RuntimeArgError, RuntimeValues, apply_runtime_values};

/// The file inside a bundle that carries the manifest (used to resolve the
/// agent home before a full unpack).
const MANIFEST_NAME: &str = "hugr.toml";

/// Parsed universal + definition-specific CLI arguments. `argv[0]` is the
/// agent's own binary name.
#[derive(Debug)]
pub struct SurfaceArgs {
    question: Option<String>,
    trace: Option<String>,
    json: bool,
    blobs: Vec<PathBuf>,
    describe: bool,
    config: bool,
    traces: bool,
    mcp_serve: bool,
    runtime: RuntimeValues,
}

/// Which audit view, if any, was requested.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Ask,
    Describe,
    Traces,
}

/// Entry point a built agent binary calls from `main`. Parses `std::env::args`,
/// runs the requested operation, prints to stdout, and returns the process exit
/// code. The ask path always returns 0.
pub async fn run_cli(bundle_bytes: &'static [u8]) -> i32 {
    run_cli_with_options(bundle_bytes, RuntimeOptions::default()).await
}

/// Entry point for a built agent binary that links agent-owned Rust response
/// types. The surface remains universal; only the registry differs.
pub async fn run_cli_with_options(bundle_bytes: &'static [u8], options: RuntimeOptions) -> i32 {
    let started = Instant::now();
    match prepare_definition(bundle_bytes) {
        Ok(def) => {
            run_definition_args_with_options(def, std::env::args_os().skip(1), started, options)
                .await
        }
        Err(err) => print_answer(&error_answer(err.to_string(), started), true),
    }
}

/// Run the universal surface for an already-loaded definition and an argument
/// iterator. Used by built binaries, embedded Rust agents, and `hugr run`.
pub async fn run_definition_args<I, T>(base_def: AgentDefinition, argv: I, started: Instant) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    run_definition_args_with_options(base_def, argv, started, RuntimeOptions::default()).await
}

/// Run the universal surface with explicit runtime wiring.
pub async fn run_definition_args_with_options<I, T>(
    base_def: AgentDefinition,
    argv: I,
    started: Instant,
    options: RuntimeOptions,
) -> i32
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = parse_surface_args(&base_def, argv);
    let pretty = !args.json; // default pretty; --json forces compact
    let mut def = base_def.clone();

    // `--config` needs only the parsed manifest, not an assembled agent.
    if args.config {
        let _ = apply_runtime_values(&mut def, &args.runtime);
        return print_json_or_die(&config_json_with_options(&def, &options), pretty);
    }

    let mode = if args.describe {
        Mode::Describe
    } else if args.traces {
        Mode::Traces
    } else {
        Mode::Ask
    };

    // MCP serve mode is long-lived. Runtime arguments are accepted per ask via
    // the advertised tool schema, so required args are not enforced at startup.
    if args.mcp_serve {
        if let Err(err) = apply_optional_runtime_values(&mut def, &args.runtime) {
            eprintln!("error: {err}");
            return 1;
        }
        return crate::mcp_serve::serve_definition_with_options(def, options).await;
    }

    let runtime_result = if mode == Mode::Ask {
        apply_runtime_values(&mut def, &args.runtime)
    } else {
        apply_optional_runtime_values(&mut def, &args.runtime)
    };
    if let Err(err) = runtime_result {
        return audit_or_answer_error(mode, err.to_string(), started, pretty);
    }

    // Assemble the agent. Warnings go to stderr.
    let agent = match build_agent_with_options(&def, &options).await {
        Ok((agent, warnings)) => {
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            agent
        }
        Err(err) => return audit_or_answer_error(mode, err.to_string(), started, pretty),
    };

    match mode {
        Mode::Describe => print_json_or_die(&agent.describe(), pretty),
        Mode::Traces => match agent.traces() {
            Ok(heads) => print_json_or_die(&heads, pretty),
            Err(err) => {
                eprintln!("error: listing traces: {err}");
                1
            }
        },
        Mode::Ask => {
            run_ask(
                &agent,
                args.question,
                args.trace,
                &args.blobs,
                started,
                pretty,
            )
            .await
        }
    }
}

fn prepare_definition(bundle_bytes: &[u8]) -> Result<AgentDefinition, LoadError> {
    let home = prepare_home(bundle_bytes).map_err(LoadError::Home)?;
    Ok(AgentDefinition::load(&home)?)
}

fn parse_surface_args<I, T>(def: &AgentDefinition, argv: I) -> SurfaceArgs
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = surface_command(def).get_matches_from(
        std::iter::once(OsString::from(def.agent.name.clone()))
            .chain(argv.into_iter().map(Into::into)),
    );
    let runtime = def
        .runtime
        .args
        .iter()
        .filter_map(|arg| {
            matches
                .get_one::<String>(&runtime_id(&arg.name))
                .map(|value| (arg.name.clone(), value.clone()))
        })
        .collect();
    SurfaceArgs {
        question: matches.get_one::<String>("question").cloned(),
        trace: matches.get_one::<String>("trace").cloned(),
        json: matches.get_flag("json"),
        blobs: matches
            .get_many::<PathBuf>("blob")
            .map(|paths| paths.cloned().collect())
            .unwrap_or_default(),
        describe: matches.get_flag("describe"),
        config: matches.get_flag("config"),
        traces: matches.get_flag("traces"),
        mcp_serve: matches.get_flag("mcp-serve"),
        runtime,
    }
}

fn surface_command(def: &AgentDefinition) -> Command {
    let mut command = Command::new(leak_str(def.agent.name.clone()))
        .about("A Hugr subagent. Ask it a question; errors are status:error answers.")
        .arg(
            Arg::new("trace")
                .long("trace")
                .help("Resume/fork from an existing trace id (writes a new child trace).")
                .value_name("ID"),
        )
        .arg(
            Arg::new("json")
                .long("json")
                .help("Emit compact single-line JSON (default is pretty-printed).")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("pretty")
                .long("pretty")
                .help("Pretty-print JSON output (the default).")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("blob")
                .long("blob")
                .help("Hand a local file in as an inbound blob (repeatable).")
                .value_name("PATH")
                .value_parser(clap::value_parser!(PathBuf))
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("describe")
                .long("describe")
                .help("Print the agent card (name, tools, privileges, tiers, pricing, limits).")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("config")
                .long("config")
                .help("Print the parsed manifest as JSON; secret values are never shown.")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("traces")
                .long("traces")
                .help("Print the stored traces (header-only, with lineage).")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("mcp-serve")
                .long("mcp-serve")
                .help("Run as a stdio MCP server exposing an ask tool.")
                .action(ArgAction::SetTrue),
        );

    let mut index = 1;
    for runtime in &def.runtime.args {
        let id = runtime_id(&runtime.name);
        let help = runtime.help.clone();
        let mut arg = Arg::new(leak_str(id.clone()))
            .help(help)
            .value_name(leak_str(runtime.name.to_ascii_uppercase()));
        if runtime.positional {
            arg = arg.index(index);
            index += 1;
        } else {
            arg = arg.long(leak_str(
                runtime
                    .flag
                    .as_deref()
                    .unwrap_or(&runtime.name)
                    .replace('_', "-"),
            ));
        }
        command = command.arg(arg);
    }
    command.arg(
        Arg::new("question")
            .help("The question to ask (omit when using --describe/--config/--traces).")
            .index(index),
    )
}

fn runtime_id(name: &str) -> String {
    format!("runtime:{name}")
}

fn leak_str(value: String) -> &'static str {
    Box::leak(value.into_boxed_str())
}

fn apply_optional_runtime_values(
    def: &mut AgentDefinition,
    explicit: &RuntimeValues,
) -> Result<(), RuntimeArgError> {
    let mut optional_def = def.clone();
    for arg in &mut optional_def.runtime.args {
        arg.required = false;
    }
    apply_runtime_values(&mut optional_def, explicit)?;
    *def = optional_def;
    Ok(())
}

/// Run one ask and print its answer. Missing question, blob problems, and infra
/// `AskError`s all surface as `status: "error"` answers (exit 0). Shared by the
/// built binary and `hugr run` — the one run path (ARCHITECTURE §21.1).
pub async fn run_ask(
    agent: &Agent,
    question: Option<String>,
    trace: Option<String>,
    blob_paths: &[PathBuf],
    started: Instant,
    pretty: bool,
) -> i32 {
    let Some(question) = question else {
        return print_answer(
            &error_answer(
                "no question provided (use --describe/--config/--traces for the audit views)",
                started,
            ),
            pretty,
        );
    };

    let mut blobs = Vec::with_capacity(blob_paths.len());
    for path in blob_paths {
        match blob_handle_from_path(path) {
            Ok(handle) => blobs.push(handle),
            Err(err) => return print_answer(&error_answer(err, started), pretty),
        }
    }

    let ask = Ask {
        question,
        trace_id: trace.map(TraceId::new),
        blobs,
        ..Ask::default()
    };

    match agent.ask(ask).await {
        Ok(answer) => print_answer(&answer, pretty),
        Err(err) => print_answer(&error_answer(err.to_string(), started), pretty),
    }
}

/// Programmatic one-shot ask against an embedded bundle, returning the `Answer`
/// (errors are answers, §18.1). This is the shared entry point for non-CLI
/// surfaces — the generated Python extension (T2.3) and any future language
/// binding — so the whole "bundle → assembled agent → one ask" path stays in
/// one tested place. The caller supplies primitives (no `Ask`/`clap` types), we
/// apply required runtime args, assemble with `options`, and run exactly one
/// ask. Build/runtime failures come back as `status: "error"` answers, never
/// panics or `Err`.
pub async fn ask_bundle_with_options(
    bundle_bytes: &[u8],
    options: &RuntimeOptions,
    question: String,
    trace_id: Option<String>,
    blob_paths: &[PathBuf],
    extra: serde_json::Value,
    runtime: &RuntimeValues,
) -> Answer {
    let started = Instant::now();
    let mut def = match prepare_definition(bundle_bytes) {
        Ok(def) => def,
        Err(err) => return error_answer(err.to_string(), started),
    };
    if let Err(err) = apply_runtime_values(&mut def, runtime) {
        return error_answer(err.to_string(), started);
    }
    let agent = match build_agent_with_options(&def, options).await {
        Ok((agent, warnings)) => {
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            agent
        }
        Err(err) => return error_answer(err.to_string(), started),
    };

    let mut blobs = Vec::with_capacity(blob_paths.len());
    for path in blob_paths {
        match blob_handle_from_path(path) {
            Ok(handle) => blobs.push(handle),
            Err(err) => return error_answer(err, started),
        }
    }

    let ask = Ask {
        question,
        trace_id: trace_id.map(TraceId::new),
        blobs,
        extra,
    };
    match agent.ask(ask).await {
        Ok(answer) => answer,
        Err(err) => error_answer(err.to_string(), started),
    }
}

/// The `--config` view: the parsed manifest as JSON. The API key env *name*
/// is shown, and whether it currently resolves — the secret value never is.
pub fn config_json(def: &AgentDefinition) -> serde_json::Value {
    config_json_with_options(def, &RuntimeOptions::default())
}

/// The `--config` view with a response-type registry available.
pub fn config_json_with_options(
    def: &AgentDefinition,
    options: &RuntimeOptions,
) -> serde_json::Value {
    let mut config = serde_json::json!({
        "agent": def.agent,
        "models": {
            "base_url": def.models.base_url,
            "api_key_env": def.models.api_key_env,
            "api_key_resolved": def.models.api_key_env.as_deref()
                .map(|var| std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false)),
            "default": def.default_tier(),
            "tiers": def.models.tiers,
        },
        "tools": def.tools.iter().map(|grant| serde_json::json!({
            "name": grant.name,
            "kind": grant.kind,
            "scope": grant.config,
        })).collect::<Vec<_>>(),
        "runtime": def.runtime,
        "limits": def.limits,
        "scratchpad": def.scratchpad,
        "traces": def.traces,
    });
    if let Some(schema) = response_schema_for_config(def, options) {
        config
            .as_object_mut()
            .expect("config root is an object")
            .insert("response".to_string(), schema);
    }
    config
}

fn response_schema_for_config(
    def: &AgentDefinition,
    options: &RuntimeOptions,
) -> Option<serde_json::Value> {
    def.response_schema.clone().or_else(|| {
        options
            .single_response_contract()
            .map(|contract| contract.schema)
    })
}

/// Build an inbound [`BlobHandle`] from a local path, guessing its media type
/// from the extension. The name hint is the file's own name. Shared with the
/// generated Python surface (T2.3).
pub fn blob_handle_from_path(path: &Path) -> Result<BlobHandle, String> {
    let path_str = path
        .to_str()
        .ok_or_else(|| format!("blob path is not valid UTF-8: {}", path.display()))?;
    let media = media_type_for(path);
    Ok(BlobHandle {
        blob_ref: BlobRef::Path {
            path: path_str.to_string(),
        },
        media_type: media.to_string(),
        name: path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
    })
}

/// A best-effort media type from a file extension.
fn media_type_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("md" | "markdown") => "text/markdown",
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("pdf") => "application/pdf",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        _ => "application/octet-stream",
    }
}

/// An assembled agent plus the non-fatal build warnings collected on the way.
/// Returned by [`load_agent`] — the shared entry point behind the CLI surface
/// and the generated Rust-crate surface (T2.2).
pub struct LoadedAgent {
    /// The ready-to-ask agent.
    pub agent: Agent,
    /// Non-fatal build warnings (unset api-key env, skipped grants, …).
    pub warnings: Vec<String>,
}

/// Failure to turn an embedded bundle into a runnable agent. Every variant is a
/// build-time / infrastructure problem — run failures are `Answer`s, not this.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("preparing agent home: {0}")]
    Home(std::io::Error),
    #[error("loading definition: {0}")]
    Manifest(#[from] crate::manifest::ManifestError),
    #[error("assembling agent: {0}")]
    Build(#[from] crate::runtime::RuntimeError),
}

/// Unpack an embedded definition [`bundle`] into the per-agent home and assemble
/// the typed [`Agent`]. This is the whole "a binary/crate carrying a definition
/// becomes a callable agent" path, reused by every generated surface.
pub async fn load_agent(bundle_bytes: &[u8]) -> Result<LoadedAgent, LoadError> {
    load_agent_with_options(bundle_bytes, &RuntimeOptions::default()).await
}

/// Unpack and assemble an embedded bundle with explicit runtime wiring.
pub async fn load_agent_with_options(
    bundle_bytes: &[u8],
    options: &RuntimeOptions,
) -> Result<LoadedAgent, LoadError> {
    let home = prepare_home(bundle_bytes).map_err(LoadError::Home)?;
    let def = AgentDefinition::load(&home)?;
    let (agent, warnings) = build_agent_with_options(&def, options).await?;
    Ok(LoadedAgent { agent, warnings })
}

/// Resolve the per-agent home directory and unpack the embedded agent bundle into
/// it. The definition source files are (re-)written every run — they are
/// immutable by design — while the runtime dirs (traces, scratch) are never in
/// the bundle, so persisted traces survive across runs and `--trace` resume
/// works on a machine with no repo checkout.
pub fn prepare_home(bundle_bytes: &[u8]) -> std::io::Result<PathBuf> {
    let manifest = bundle::get(bundle_bytes, MANIFEST_NAME)?.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "bundle has no hugr.toml")
    })?;
    let (name, version) = manifest_identity(&manifest);
    let home = agent_home_dir(&name, &version);
    std::fs::create_dir_all(&home)?;
    bundle::unpack(bundle_bytes, &home)?;
    Ok(home)
}

/// Pull `name`/`version` out of the manifest bytes with a forgiving parse — we
/// only need them to name the home dir, so a parse miss falls back to defaults.
fn manifest_identity(manifest: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(manifest);
    let value: toml::Value = text
        .parse()
        .unwrap_or(toml::Value::Table(Default::default()));
    let agent = value.get("agent");
    let name = agent
        .and_then(|a| a.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("agent")
        .to_string();
    let version = agent
        .and_then(|a| a.get("version"))
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    (sanitize_segment(&name), sanitize_segment(&version))
}

/// The per-agent home: `$HUGR_AGENT_HOME` if set, else `<data>/hugr/<name>@<version>`
/// where `<data>` follows XDG (`$XDG_DATA_HOME`, else `$HOME/.local/share`, else
/// the temp dir). Stable across invocations so traces persist.
fn agent_home_dir(name: &str, version: &str) -> PathBuf {
    if let Ok(explicit) = std::env::var("HUGR_AGENT_HOME") {
        if !explicit.is_empty() {
            return PathBuf::from(explicit);
        }
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    data.join("hugr").join(format!("{name}@{version}"))
}

/// Reduce a manifest string to a single safe path segment.
fn sanitize_segment(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "agent".to_string()
    } else {
        cleaned
    }
}

/// For an audit view, a preparation failure is a stderr error + non-zero exit;
/// for the ask path it is an error answer at exit 0 (§18.1).
fn audit_or_answer_error(mode: Mode, message: String, started: Instant, pretty: bool) -> i32 {
    if mode == Mode::Ask {
        print_answer(&error_answer(message, started), pretty)
    } else {
        eprintln!("error: {message}");
        1
    }
}

/// An error-status [`Answer`] stamped with the elapsed duration. Shared by
/// every surface that must turn a failure into an answer (§18.1).
pub fn error_answer(message: impl Into<String>, started: Instant) -> Answer {
    Answer {
        status: STATUS_ERROR.to_string(),
        response: json!({ "error": message.into() }),
        metadata: AnswerMeta {
            duration_ms: started.elapsed().as_millis() as u64,
            ..AnswerMeta::default()
        },
        ..Answer::default()
    }
}

/// Print one JSON answer to stdout. The ask path always exits 0 — errors are
/// answers.
pub fn print_answer(answer: &Answer, pretty: bool) -> i32 {
    print_json_or_die(answer, pretty);
    0 // the ask path always exits 0 — errors are answers
}

/// Serialize to stdout. Returns exit 0 on success, 1 if serialization fails
/// (used directly by the audit views).
fn print_json_or_die<T: serde::Serialize>(value: &T, pretty: bool) -> i32 {
    let result = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    };
    match result {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(err) => {
            eprintln!("error: serializing output: {err}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scaffold::{Template, scaffold_files};

    /// Build a bundle in memory from a scaffolded definition written to a temp
    /// dir, so surface behavior is testable without `cargo build`.
    fn bundle_for(template: Template, name: &str) -> (Vec<u8>, PathBuf) {
        let root = std::env::temp_dir().join(format!("hugr-surface-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for file in scaffold_files(name, template) {
            let path = root.join(&file.rel_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, file.contents).unwrap();
        }
        let bytes = bundle::pack(&root, &[".hugr-traces", ".scratch"]).unwrap();
        (bytes, root)
    }

    #[test]
    fn config_json_shows_key_env_name_but_never_the_secret() {
        unsafe { std::env::set_var("HUGR_CFG_TEST_KEY", "super-secret-value") };
        let src = "[agent]\nname = \"x\"\n[models]\napi_key_env = \"HUGR_CFG_TEST_KEY\"\n[models.medium]\nmodel = \"m\"\n[tools.fs_read]\nroot = \"./policies\"\n";
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let cfg = config_json(&def);
        assert_eq!(cfg["models"]["api_key_env"], "HUGR_CFG_TEST_KEY");
        assert_eq!(cfg["models"]["api_key_resolved"], true);
        assert_eq!(cfg["tools"][0]["name"], "fs_read");
        assert!(!cfg.to_string().contains("super-secret-value"));
        assert!(cfg.get("response").is_none());
        unsafe { std::env::remove_var("HUGR_CFG_TEST_KEY") };
    }

    #[derive(serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
    struct ConfigResponse {
        response: String,
    }

    #[test]
    fn config_json_shows_actual_response_schema_when_registered() {
        let src = "[agent]\nname = \"x\"\n[models.medium]\nmodel = \"m\"\n";
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let options = RuntimeOptions::new().with_response_type::<ConfigResponse>(
            "test_agent::ConfigResponse",
            "test_agent__ConfigResponse",
        );

        let cfg = config_json_with_options(&def, &options);
        assert_eq!(cfg["response"]["type"], "object");
        assert_eq!(cfg["response"]["properties"]["response"]["type"], "string");
        assert!(
            !cfg["response"]
                .as_object()
                .unwrap()
                .contains_key("schema_loaded")
        );
    }

    #[test]
    fn runtime_positional_is_parsed_before_question() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[tools.fs_read]
root = "."
[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let args = parse_surface_args(&def, ["./manual", "what changed?", "--json"]);
        assert_eq!(args.runtime["docs_path"], "./manual");
        assert_eq!(args.question.as_deref(), Some("what changed?"));
        assert!(args.json);
    }

    #[test]
    fn manifest_identity_reads_name_and_version() {
        let (id_name, id_version) =
            manifest_identity(b"[agent]\nname='my agent'\nversion='1.2.3'\n");
        assert_eq!(id_name, "my_agent");
        assert_eq!(id_version, "1.2.3");
        // Missing fields fall back.
        let (n, v) = manifest_identity(b"garbage = ");
        assert_eq!((n.as_str(), v.as_str()), ("agent", "0.0.0"));
    }

    #[tokio::test]
    async fn prepare_home_unpacks_and_agent_describes() {
        let (bytes, src) = bundle_for(Template::Blank, "surfacedesc");
        let home = std::env::temp_dir().join(format!("hugr-home-desc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        // Point the home dir at a temp location via the runtime resolver.
        unsafe { std::env::set_var("HUGR_AGENT_HOME", &home) };
        let prepared = prepare_home(&bytes).unwrap();
        assert_eq!(prepared, home);
        assert!(home.join("hugr.toml").exists());

        let def = AgentDefinition::load(&home).unwrap();
        let (agent, _) = crate::runtime::build_agent(&def).await.unwrap();
        let card = agent.describe();
        assert_eq!(card.name, "surfacedesc");
        let tools: Vec<_> = card.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tools.contains(&"scratch_write"), "{tools:?}");

        unsafe { std::env::remove_var("HUGR_AGENT_HOME") };
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&home);
    }
}
