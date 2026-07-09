//! Interpret a definition: assemble a `hugr-agent` [`Agent`] from a parsed
//! [`AgentDefinition`] (ROADMAP T1.3, ARCHITECTURE §20.4).
//!
//! This is the "interpreter mode" every definition gets before any bundling
//! (`hugr build`, T2): [`build_agent`] wires the model tiers (one
//! OpenAI-compatible adapter per `[models.<tier>]`), the pricing table, the
//! granted library tools (sandbox-by-registration — only what the manifest
//! grants is registered), the system prompt (with a small template-var set),
//! the declared limits, and the trace/scratch locations. `hugr run` then does
//! one [`Agent::ask`].
//!
//! `[tools.mcp.<name>]` grants (§20.3) are wired here (ROADMAP T1.5): each
//! connects its external process and registers the discovered tools.
//! Agent-as-tool grants (`[tools.agent.<name>]`, §20.5) are subprocess-only:
//! `artifact` names a built agent binary spoken to over the CLI JSON contract.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hugr_agent::{
    Agent, AgentLimits, AgentToolResolver, AgentToolSpec, Answer, AnswerHook, Ask, AskHook,
    Pricing, TraceStore, depth_exceeded_resolver,
};
use hugr_core::{ModelSelector, SamplingParams};
use hugr_host::mcp::{McpError, McpServerConfig, load_stdio};
use hugr_providers::OpenAiAdapter;
use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};

use crate::manifest::{AgentDefinition, ToolGrant, ToolKind};
use crate::tools::{self, ToolError};

pub use hugr_agent::ResponseContract;

/// Default trace-store directory when the manifest omits `[traces].store`.
pub const DEFAULT_TRACE_DIRNAME: &str = ".hugr-traces";

/// The trace store a definition reads/writes, resolved the same way
/// [`build_agent`] resolves it (`[traces].store` against the agent crate folder,
/// else `.hugr-traces`). Trace tooling (`hugr traces`/`replay`/`verify`) points
/// at this store (ROADMAP T1.7).
pub fn trace_store_for(def: &AgentDefinition) -> TraceStore {
    let base_dir = def.source_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    let dir = def
        .traces
        .store
        .as_deref()
        .map(|s| resolve(&base_dir, s))
        .unwrap_or_else(|| base_dir.join(DEFAULT_TRACE_DIRNAME));
    TraceStore::new(dir)
}

/// Failure to assemble a runtime from a definition. (Run failures are
/// *answers*, §18.1 — this is strictly build-time.)
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// The definition declares no model tier.
    #[error("definition has no [models] tier to run")]
    NoModel,
    /// A granted library tool could not be constructed (bad scope, missing
    /// root/file, …).
    #[error(transparent)]
    Tool(#[from] ToolError),
    /// An external tool grant is missing its `command`.
    #[error("tool grant `{0}` is missing string `command`")]
    MissingCommand(String),
    /// A granted MCP server could not be loaded (spawn/handshake/discovery).
    #[error("loading MCP server `{name}`: {source}")]
    Mcp {
        name: String,
        #[source]
        source: McpError,
    },
    /// A granted child agent (`[tools.agent.<name>]`) could not be resolved
    /// (missing or bad `artifact` path).
    #[error("wiring agent-as-tool grant `{name}`: {message}")]
    Agent { name: String, message: String },
    /// Definition-level runtime wiring failed.
    #[error("{message}")]
    Definition { message: String },
}

/// Default recursion cap for agent-as-tool delegation (ARCHITECTURE §20.5,
/// §13): how many nested `agent_<name>` calls a delegation chain may make
/// before a grant is replaced by an `agent_depth_exceeded` stub. The remaining
/// budget rides [`AGENT_DEPTH_ENV`] across the subprocess boundary, so cycles
/// (`a` grants `b` grants `a`) terminate.
pub const DEFAULT_MAX_AGENT_DEPTH: u32 = 3;

/// Env var carrying the remaining agent-as-tool depth budget into a spawned
/// child artifact (§20.5). Absent = [`DEFAULT_MAX_AGENT_DEPTH`].
pub const AGENT_DEPTH_ENV: &str = "HUGR_AGENT_DEPTH";

/// Runtime wiring supplied by an embedding agent crate or a generated build
/// shim. This keeps `hugr-toolkit` generic: it can parse the manifest without
/// depending on the agent crate, while a process that links the agent crate
/// registers the actual Rust response type.
#[derive(Clone, Debug, Default)]
pub struct RuntimeOptions {
    response_contracts: std::collections::BTreeMap<String, ResponseContract>,
    ask_hooks: Vec<AskHook>,
    answer_hooks: Vec<AnswerHook>,
}

impl RuntimeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_response_contract(
        mut self,
        rust_type: impl Into<String>,
        contract: ResponseContract,
    ) -> Self {
        self.response_contracts.insert(rust_type.into(), contract);
        self
    }

    pub fn with_response_type<T>(
        self,
        rust_type: impl Into<String>,
        schema_name: impl Into<String>,
    ) -> Self
    where
        T: DeserializeOwned + Serialize + JsonSchema + 'static,
    {
        self.with_response_contract(rust_type, ResponseContract::from_type::<T>(schema_name))
    }

    pub fn with_ask_hook(mut self, hook: AskHook) -> Self {
        self.ask_hooks.push(hook);
        self
    }

    pub fn with_ask_hooks(mut self, hooks: impl IntoIterator<Item = AskHook>) -> Self {
        self.ask_hooks.extend(hooks);
        self
    }

    pub fn with_answer_hook(mut self, hook: AnswerHook) -> Self {
        self.answer_hooks.push(hook);
        self
    }

    pub fn with_answer_hooks(mut self, hooks: impl IntoIterator<Item = AnswerHook>) -> Self {
        self.answer_hooks.extend(hooks);
        self
    }

    pub fn response_contract(&self, rust_type: &str) -> Option<ResponseContract> {
        self.response_contracts.get(rust_type).cloned()
    }

    pub fn single_response_contract(&self) -> Option<ResponseContract> {
        (self.response_contracts.len() == 1)
            .then(|| self.response_contracts.values().next().cloned())
            .flatten()
    }

    pub fn ask_hooks(&self) -> Vec<AskHook> {
        self.ask_hooks.clone()
    }

    pub fn answer_hooks(&self) -> Vec<AnswerHook> {
        self.answer_hooks.clone()
    }
}

/// Assemble a [`Agent`] from a definition, collecting non-fatal build warnings
/// (e.g. an external-tool grant that is not yet wired). Relative scopes resolve
/// against the agent's `source_dir` (else the process cwd). This is the
/// one assembly path (`hugr run`, the built binary, `--mcp-serve`, hugr-docs).
pub async fn build_agent(def: &AgentDefinition) -> Result<(Agent, Vec<String>), RuntimeError> {
    build_agent_with_options(def, &RuntimeOptions::default()).await
}

/// Assemble an [`Agent`] with explicit runtime wiring supplied by an embedding
/// crate or generated shim.
pub async fn build_agent_with_options(
    def: &AgentDefinition,
    options: &RuntimeOptions,
) -> Result<(Agent, Vec<String>), RuntimeError> {
    let mut warnings = Vec::new();
    let base_dir = def.source_dir.clone().unwrap_or_else(|| PathBuf::from("."));

    if def.models.tiers.is_empty() {
        return Err(RuntimeError::NoModel);
    }

    // Trace store: [traces].store, resolved against the agent crate folder.
    let store = trace_store_for(def);

    let version = if def.agent.version.trim().is_empty() {
        "0.0.0"
    } else {
        def.agent.version.as_str()
    };
    let mut agent = Agent::new(def.agent.name.clone(), version, store);
    agent.description = def.agent.description.clone();

    // The provider API key rides an env var (§20.1) — never the manifest. When
    // unset, the adapter gets an empty key and the run fails as an error answer.
    let api_key = def
        .models
        .api_key_env
        .as_deref()
        .and_then(|var| std::env::var(var).ok())
        .unwrap_or_default();
    if let Some(var) = &def.models.api_key_env
        && api_key.is_empty()
    {
        warnings.push(format!(
            "api key env var `{var}` is unset; model calls will fail until it is set"
        ));
    }

    let mut pricing = Pricing::new();
    for (tier_name, tier) in &def.models.tiers {
        let selector = ModelSelector::named(tier_name.clone());
        let mut adapter = OpenAiAdapter::new(api_key.clone(), tier.model.clone());
        if let Some(base) = &def.models.base_url {
            adapter = adapter.with_base_url(base.clone());
        }
        let mut sampling = SamplingParams::new();
        if let Some(t) = tier.temperature {
            sampling = sampling.with_temperature(t as f32);
        }
        if let Some(m) = tier.max_tokens {
            sampling = sampling.with_max_tokens(m);
        }
        adapter = adapter.with_default_params(sampling);
        agent.models.push((selector, Arc::new(adapter)));

        // Price a tier that declares either side; a missing side is 0.
        if tier.input_usd_per_m_tokens.is_some() || tier.output_usd_per_m_tokens.is_some() {
            pricing = pricing.with_tier(
                tier_name.clone(),
                tier.input_usd_per_m_tokens.unwrap_or(0.0),
                tier.output_usd_per_m_tokens.unwrap_or(0.0),
            );
        }
    }
    if let Some(default) = def.default_tier() {
        agent.default_model = Some(ModelSelector::named(default.to_string()));
    }
    agent.pricing = pricing;
    agent.response_contract = response_contract(def, options)?;
    agent.ask_hooks = options.ask_hooks();
    agent.answer_hooks = options.answer_hooks();

    // System prompt (with template vars). A definition without SYSTEM.md gets a
    // minimal default so the agent still runs.
    agent.system_prompt = Some(render_system_prompt(def));

    // Granted tools — sandbox-by-registration (§20.1). Library grants build
    // in-process; MCP grants (§20.3) connect their external process and
    // register the discovered tools. Agent-as-tool grants (§20.5) are T3.8.
    for grant in &def.tools {
        match grant.kind {
            ToolKind::Library => {
                for capability in tools::build_library_grant(grant, &base_dir)? {
                    agent.capabilities.push(capability);
                }
            }
            ToolKind::Mcp => {
                let (command, args) = command_and_args(grant)?;
                let config = McpServerConfig::new(grant.name.clone(), command).args(args);
                let caps = load_stdio(config)
                    .await
                    .map_err(|source| RuntimeError::Mcp {
                        name: grant.name.clone(),
                        source,
                    })?;
                agent.capabilities.extend(caps);
            }
            ToolKind::Agent => agent.agent_tools.push(build_agent_tool(grant, &base_dir)?),
        }
    }

    // Declared limits, enforced host-side per ask by `hugr-agent` (T3.1).
    let mut limits = AgentLimits::new();
    if let Some(v) = def.limits.max_model_calls {
        limits = limits.with_max_model_calls(v);
    }
    if let Some(v) = def.limits.max_cost_micro_usd {
        limits = limits.with_max_cost_micro_usd(v);
    }
    if let Some(v) = def.limits.timeout_s {
        limits = limits.with_timeout_ms(v.saturating_mul(1000));
    }
    agent.limits = limits;

    if let Some(root) = &def.scratchpad.root {
        agent.scratch_root = resolve(&base_dir, root);
    }

    Ok((agent, warnings))
}

fn response_contract(
    def: &AgentDefinition,
    options: &RuntimeOptions,
) -> Result<Option<ResponseContract>, RuntimeError> {
    Ok(def
        .response_schema
        .as_ref()
        .map(|schema| ResponseContract::from_schema("agent_response", schema.clone()))
        .or_else(|| options.single_response_contract()))
}

/// Extract `command` (required) and `args` (optional string array) from an
/// external tool grant's config.
fn command_and_args(grant: &ToolGrant) -> Result<(String, Vec<String>), RuntimeError> {
    let command = grant
        .config
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| RuntimeError::MissingCommand(grant.name.clone()))?
        .to_string();
    let args = grant
        .config
        .get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok((command, args))
}

/// The remaining agent-as-tool depth budget of *this* process (§20.5): read
/// from [`AGENT_DEPTH_ENV`] (stamped by the spawning parent), defaulting to
/// [`DEFAULT_MAX_AGENT_DEPTH`] at the root of a delegation chain.
fn remaining_agent_depth() -> u32 {
    std::env::var(AGENT_DEPTH_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_AGENT_DEPTH)
}

/// Wire one `[tools.agent.<name>]` grant into an `agent_<name>` tool spec
/// (ARCHITECTURE §20.5, ROADMAP T3.8). Subprocess-only: `artifact` names a
/// built agent binary, spawned per call over the CLI JSON contract (§21.1).
/// At zero remaining depth the grant becomes an `agent_depth_exceeded` stub
/// (the cycle cut); otherwise the child is spawned with one less budget.
fn build_agent_tool(grant: &ToolGrant, base_dir: &Path) -> Result<AgentToolSpec, RuntimeError> {
    let tool_name = format!("agent_{}", grant.name);
    let err = |message: String| RuntimeError::Agent {
        name: grant.name.clone(),
        message,
    };

    // Depth/cycle cut: no child is ever run.
    let depth = remaining_agent_depth();
    if depth == 0 {
        return Ok(AgentToolSpec::new(
            &tool_name,
            "delegation refused: max agent depth reached",
            depth_exceeded_resolver(grant.name.clone()),
        ));
    }

    let artifact = grant
        .config
        .get("artifact")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err("missing string `artifact` (path to a built agent binary)".into()))?;
    let resolved = resolve(base_dir, artifact);
    if !resolved.is_file() {
        return Err(err(format!(
            "`artifact` does not resolve to a built agent binary: {}",
            resolved.display()
        )));
    }

    let bin = resolved.clone();
    let resolver: AgentToolResolver = Arc::new(move |ask: Ask| {
        let bin = bin.clone();
        Box::pin(async move { run_subprocess_agent(&bin, ask, depth - 1).await })
    });
    Ok(AgentToolSpec::new(
        tool_name,
        format!("subagent artifact at {}", resolved.display()),
        resolver,
    ))
}

/// Run a built agent artifact as a subprocess over the CLI JSON contract
/// (§21.1): `<bin> <question> --json [--trace <id>]`, then parse the `Answer`
/// from stdout. The child inherits `depth` via [`AGENT_DEPTH_ENV`] so a
/// delegation cycle terminates. Blob forwarding is a later refinement.
async fn run_subprocess_agent(bin: &Path, ask: Ask, depth: u32) -> Result<Answer, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(&ask.question).arg("--json");
    cmd.env(AGENT_DEPTH_ENV, depth.to_string());
    if let Some(trace_id) = &ask.trace_id {
        cmd.arg("--trace").arg(trace_id.as_str());
    }
    let output = cmd
        .output()
        .await
        .map_err(|e| format!("spawning subagent `{}`: {e}", bin.display()))?;
    if !output.status.success() {
        // The CLI contract always exits 0 with a JSON answer; a non-zero exit is
        // an infrastructure failure surfaced to the calling model.
        return Err(format!(
            "subagent `{}` exited with {}: {}",
            bin.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    serde_json::from_slice::<Answer>(&output.stdout)
        .map_err(|e| format!("parsing subagent answer: {e}"))
}

/// Resolve a manifest path against the agent crate folder (absolute paths pass
/// through).
fn resolve(base_dir: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// Render the system prompt, substituting the supported template vars:
/// `{{agent_name}}`, `{{tools}}` (comma-joined tool names), `{{date}}`
/// (UTC `YYYY-MM-DD`). A definition with no `SYSTEM.md` gets a minimal default.
pub fn render_system_prompt(def: &AgentDefinition) -> String {
    let base = def.system_prompt.clone().unwrap_or_else(|| {
        format!(
            "You are {}, a focused subagent. Answer the user's question using only the provided tools.",
            def.agent.name
        )
    });
    base.replace("{{agent_name}}", &def.agent.name)
        .replace("{{tools}}", &tool_names(def).join(", "))
        .replace("{{date}}", &utc_date())
}

/// The concrete tool names a definition exposes (library grants expanded to
/// their capability names, plus the always-present scratchpad tools).
fn tool_names(def: &AgentDefinition) -> Vec<String> {
    let mut names = Vec::new();
    for grant in &def.tools {
        match grant.kind {
            ToolKind::Library => match tools::spec(&grant.name) {
                Some(spec) => names.extend(spec.tools.iter().map(|t| t.to_string())),
                None => names.push(grant.name.clone()),
            },
            _ => names.push(grant.name.clone()),
        }
    }
    for scratch in ["scratch_read", "scratch_write", "scratch_list"] {
        names.push(scratch.to_string());
    }
    names.sort();
    names.dedup();
    names
}

/// Current UTC date as `YYYY-MM-DD`, computed from the wall clock with the
/// civil-from-days algorithm (no `chrono` dependency). Host-side only — the
/// prompt is recorded in the trace, so replay reuses the recorded value.
fn utc_date() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86_400) as i64;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 → (year, month,
/// day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AgentDefinition;

    const DEF: &str = r#"
[agent]
name = "policy-docs"
[models.medium]
model = "m"
[tools.fs_read]
root = "."
"#;

    #[test]
    fn renders_template_vars() {
        let mut def = AgentDefinition::parse(DEF, "hugr.toml").unwrap();
        def.system_prompt =
            Some("Agent {{agent_name}} has tools: {{tools}}. Today is {{date}}.".into());
        let prompt = render_system_prompt(&def);
        assert!(prompt.contains("Agent policy-docs has tools:"));
        assert!(prompt.contains("fs_read"));
        assert!(prompt.contains("scratch_read"));
        assert!(!prompt.contains("{{"), "all vars substituted: {prompt}");
    }

    #[test]
    fn civil_date_matches_known_epochs() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(18_993), (2022, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[tokio::test]
    async fn builds_an_agent_with_library_tools() {
        // Use a real, existing dir so fs_read's root canonicalizes.
        let mut def = AgentDefinition::parse(DEF, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        // fs_read root "." resolves to temp_dir (exists).
        let (agent, warnings) = build_agent(&def).await.unwrap();
        let card = agent.describe();
        assert_eq!(card.name, "policy-docs");
        // The six fs_read tools plus the three scratch tools are on the card.
        let tool_names: Vec<_> = card.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"fs_read"));
        assert!(tool_names.contains(&"fs_search"));
        assert!(tool_names.contains(&"scratch_write"));
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Write a minimal agent crate folder and return its path.
    fn write_def(tag: &str, toml: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hugr-agtool-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"test-agent\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("hugr.toml"), toml).unwrap();
        dir
    }

    #[tokio::test]
    async fn agent_as_tool_artifact_grant_registers_agent_tool() {
        // A built-artifact grant (§20.5, subprocess-only) registers an
        // `agent_<name>` capability on the parent's card.
        let dir = write_def("artifact", "");
        let artifact = dir.join("child-bin");
        std::fs::write(&artifact, b"#!/bin/sh\n").unwrap();
        let parent_src = format!(
            "[agent]\nname = \"parent\"\n[models.medium]\nmodel = \"m\"\n[tools.agent.helper]\nartifact = {:?}\n",
            artifact.display().to_string()
        );
        let mut def = AgentDefinition::parse(&parent_src, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        let (agent, warnings) = build_agent(&def).await.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<_> = agent
            .describe()
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        assert!(names.contains(&"agent_helper".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn agent_as_tool_bad_artifact_is_a_build_error() {
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"
[tools.agent.receipts]
artifact = "./does-not-exist"
"#;
        let mut def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        let err = build_agent(&def)
            .await
            .err()
            .expect("bad artifact → build error");
        assert!(matches!(err, RuntimeError::Agent { .. }), "{err}");
    }

    #[tokio::test]
    async fn mcp_grant_missing_command_is_a_build_error() {
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"
[tools.mcp.docs]
args = ["--stdio"]
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let err = build_agent(&def)
            .await
            .err()
            .expect("missing command errors");
        assert!(matches!(err, RuntimeError::MissingCommand(_)), "{err}");
    }
}
