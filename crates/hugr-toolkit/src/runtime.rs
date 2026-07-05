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
//! `[tools.mcp.<name>]` and `[tools.plugin.<name>]` grants (§20.3) are wired
//! here (ROADMAP T1.5): each connects its external process and registers the
//! discovered tools. Agent-as-tool grants (`[tools.agent.<name>]`, §20.5) still
//! warn and are skipped until ROADMAP T3.8.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hugr_agent::{
    Agent, AgentLimits, AgentToolResolver, AgentToolSpec, Answer, Ask, ConfigEntry,
    ConfigProvenance, Pricing, TraceStore, depth_exceeded_resolver,
};
use hugr_core::{ModelSelector, SamplingParams};
use hugr_host::PluginError;
use hugr_host::mcp::{McpError, McpServerConfig, load_stdio};
use hugr_host::plugins::load_subprocess;
use hugr_host::policy::AllowAll;
use hugr_providers::OpenAiAdapter;

use crate::manifest::{AgentDefinition, ToolGrant, ToolKind};
use crate::tools::{self, ToolError};

/// Default trace-store directory when the manifest omits `[traces].store`.
pub const DEFAULT_TRACE_DIRNAME: &str = ".hugr-traces";

/// The trace store a definition reads/writes, resolved the same way
/// [`build_agent`] resolves it (`[traces].store` against the definition folder,
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
#[non_exhaustive]
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
    /// A granted subprocess plugin could not be loaded.
    #[error("loading plugin `{name}`: {source}")]
    Plugin {
        name: String,
        #[source]
        source: PluginError,
    },
    /// A granted child agent (`[tools.agent.<name>]`) could not be resolved
    /// (bad `ref`, unloadable child definition).
    #[error("wiring agent-as-tool grant `{name}`: {message}")]
    Agent { name: String, message: String },
}

/// Default recursion cap for agent-as-tool delegation (ARCHITECTURE §20.5,
/// §13): how many nested `agent_<name>` calls a definition tree may build
/// before a grant is replaced by an `agent_depth_exceeded` stub. Bounds cycles
/// (`a` grants `b` grants `a`) statically at build time.
pub const DEFAULT_MAX_AGENT_DEPTH: u32 = 3;

/// Assemble a [`Agent`] from a definition, collecting non-fatal build warnings
/// (e.g. an external-tool grant that is not yet wired). Relative scopes resolve
/// against the definition's `source_dir` (else the process cwd).
pub async fn build_agent(def: &AgentDefinition) -> Result<(Agent, Vec<String>), RuntimeError> {
    build_agent_depth_with_provider_key(def, DEFAULT_MAX_AGENT_DEPTH, None).await
}

/// Depth-aware assembly (ARCHITECTURE §20.5, ROADMAP T3.8): `agent_depth` is the
/// remaining agent-as-tool recursion budget. A granted child is built with one
/// less; at zero, the grant becomes an `agent_depth_exceeded` stub.
pub async fn build_agent_depth(
    def: &AgentDefinition,
    agent_depth: u32,
) -> Result<(Agent, Vec<String>), RuntimeError> {
    build_agent_depth_with_provider_key(def, agent_depth, None).await
}

/// Assemble a definition with a provider key supplied by the caller instead of
/// the manifest's `api_key_env`. This is for compatibility surfaces that
/// already accepted explicit secrets before they became thin wrappers over the
/// definition runtime (ROADMAP T1.6/T2.3); it avoids mutating process-global env.
pub async fn build_agent_with_provider_key(
    def: &AgentDefinition,
    provider_api_key: impl Into<String>,
) -> Result<(Agent, Vec<String>), RuntimeError> {
    build_agent_depth_with_provider_key(def, DEFAULT_MAX_AGENT_DEPTH, Some(provider_api_key.into()))
        .await
}

async fn build_agent_depth_with_provider_key(
    def: &AgentDefinition,
    agent_depth: u32,
    provider_api_key: Option<String>,
) -> Result<(Agent, Vec<String>), RuntimeError> {
    let mut warnings = Vec::new();
    let base_dir = def.source_dir.clone().unwrap_or_else(|| PathBuf::from("."));

    if def.models.tiers.is_empty() {
        return Err(RuntimeError::NoModel);
    }

    // Trace store: [traces].store, resolved against the definition folder.
    let store = trace_store_for(def);

    let version = if def.agent.version.trim().is_empty() {
        "0.0.0"
    } else {
        def.agent.version.as_str()
    };
    let mut builder = Agent::builder(def.agent.name.clone(), version, store)
        .description(def.agent.description.clone())
        .policy(Arc::new(AllowAll));

    // The provider API key rides an env var (§20.1) — never the manifest. When
    // unset, the adapter gets an empty key and the run fails as an error answer.
    let api_key = provider_api_key.clone().unwrap_or_else(|| {
        def.models
            .api_key_env
            .as_deref()
            .and_then(|var| std::env::var(var).ok())
            .unwrap_or_default()
    });
    if provider_api_key.is_none()
        && let Some(var) = &def.models.api_key_env
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
        builder = builder.model(selector, Arc::new(adapter));

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
        builder = builder.default_model(ModelSelector::named(default.to_string()));
    }
    builder = builder.pricing(pricing);

    // System prompt (with template vars). A definition without SYSTEM.md gets a
    // minimal default so the agent still runs.
    let prompt = render_system_prompt(def);
    builder = builder.system_prompt(prompt);

    // Granted tools — sandbox-by-registration (§20.1). Library grants build
    // in-process; MCP/plugin grants (§20.3) connect their external process and
    // register the discovered tools. Agent-as-tool grants (§20.5) are T3.8.
    for grant in &def.tools {
        match grant.kind {
            ToolKind::Library => {
                // A scope of `group:<name>` (§18.5, T3.7) defers registration:
                // the tool is bound to that group and registered only per-ask,
                // when a matching grant arrives. Otherwise the manifest scope is
                // concrete and the capability is registered eagerly.
                if let Some(group) = tools::group_scope(grant) {
                    builder =
                        builder.group_binding(tools::library_group_binding(grant, group, &base_dir));
                } else {
                    for capability in tools::build_library_grant(grant, &base_dir)? {
                        builder = builder.capability(capability);
                    }
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
                for capability in caps {
                    builder = builder.capability(capability);
                }
            }
            ToolKind::Plugin => {
                let (command, args) = command_and_args(grant)?;
                let caps = load_subprocess(command, args).await.map_err(|source| {
                    RuntimeError::Plugin {
                        name: grant.name.clone(),
                        source,
                    }
                })?;
                for capability in caps {
                    builder = builder.capability(capability);
                }
            }
            ToolKind::Agent => {
                match build_agent_tool(grant, &base_dir, agent_depth).await {
                    Ok(spec) => builder = builder.agent_tool(spec),
                    Err(err) => return Err(err),
                }
            }
        }
    }

    // Declared limits, enforced host-side per ask by `hugr-agent` (T3.1).
    let mut limits = AgentLimits::new();
    if let Some(v) = def.limits.max_turns {
        limits = limits.with_max_turns(v);
    }
    if let Some(v) = def.limits.max_model_calls {
        limits = limits.with_max_model_calls(v);
    }
    if let Some(v) = def.limits.max_cost_micro_usd {
        limits = limits.with_max_cost_micro_usd(v);
    }
    if let Some(v) = def.limits.timeout_s {
        limits = limits.with_timeout_ms(v.saturating_mul(1000));
    }
    builder = builder.limits(limits);

    if let Some(root) = &def.scratchpad.root {
        builder = builder.scratch_root(resolve(&base_dir, root));
    }

    // Structured-answer-extra schema (T3.4): inline `[answer].extra_schema` or a
    // `.json` file. Advisory only — a schema that can't be read/parsed warns and
    // is skipped rather than failing the build.
    match resolve_answer_schema(def, &base_dir) {
        Ok(Some(schema)) => builder = builder.answer_schema(schema),
        Ok(None) => {}
        Err(warning) => warnings.push(warning),
    }

    // Effective config with real provenance + redaction for `--config` (T3.5).
    builder = builder.config_entries(effective_config(def, &base_dir));

    Ok((builder.build(), warnings))
}

/// Build the effective configuration with per-key provenance and secret
/// redaction (ROADMAP T3.5) — the machine-readable audit surface behind
/// `<agent> --config`. Values present in the manifest are tagged `Manifest`;
/// values that fall back are `Default`; the provider secret is resolved from
/// its env var (`Env`) and **redacted** — the manifest only ever carries the
/// var *name*, never the key. Deterministic order (identity → models → tools →
/// limits → scratch/traces → answer).
pub fn effective_config(def: &AgentDefinition, base_dir: &Path) -> Vec<ConfigEntry> {
    use ConfigProvenance::{Default, Env, Manifest};
    let manifest_or_default = |present: bool| if present { Manifest } else { Default };

    let mut entries = Vec::new();

    // Identity.
    entries.push(ConfigEntry::visible(
        "agent.name",
        def.agent.name.clone(),
        Manifest,
    ));
    entries.push(ConfigEntry::visible(
        "agent.version",
        if def.agent.version.trim().is_empty() {
            "0.0.0".to_string()
        } else {
            def.agent.version.clone()
        },
        manifest_or_default(!def.agent.version.trim().is_empty()),
    ));
    entries.push(ConfigEntry::visible(
        "agent.description",
        def.agent.description.clone(),
        manifest_or_default(!def.agent.description.is_empty()),
    ));

    // Models: base_url, api key (name from manifest, secret from env/redacted),
    // default tier, and one entry per declared tier.
    if let Some(base) = &def.models.base_url {
        entries.push(ConfigEntry::visible("models.base_url", base.clone(), Manifest));
    }
    if let Some(var) = &def.models.api_key_env {
        // The env var *name* is not a secret — surface it (Manifest).
        entries.push(ConfigEntry::visible(
            "models.api_key_env",
            var.clone(),
            Manifest,
        ));
        // Whether it resolved is useful, non-secret audit info (Env).
        entries.push(ConfigEntry::visible(
            "models.api_key_resolved",
            std::env::var(var).map(|v| !v.is_empty()).unwrap_or(false),
            Env,
        ));
        // The secret itself is redacted and never printed.
        entries.push(ConfigEntry::redacted("models.api_key", Env));
    }
    entries.push(ConfigEntry::visible(
        "models.default",
        def.default_tier().unwrap_or("").to_string(),
        manifest_or_default(def.models.default.is_some()),
    ));
    for (tier_name, tier) in &def.models.tiers {
        entries.push(ConfigEntry::visible(
            format!("models.{tier_name}"),
            serde_json::json!({
                "model": tier.model,
                "input_usd_per_m_tokens": tier.input_usd_per_m_tokens,
                "output_usd_per_m_tokens": tier.output_usd_per_m_tokens,
                "temperature": tier.temperature,
                "max_tokens": tier.max_tokens,
            }),
            Manifest,
        ));
    }

    // Tools (kind + scope) — the audit surface for blast radius.
    for grant in &def.tools {
        entries.push(ConfigEntry::visible(
            format!("tools.{}", grant.name),
            serde_json::json!({
                "kind": format!("{:?}", grant.kind).to_lowercase(),
                "scope": grant.config,
            }),
            Manifest,
        ));
    }

    // Limits.
    let any_limit = def.limits.max_turns.is_some()
        || def.limits.max_model_calls.is_some()
        || def.limits.max_cost_micro_usd.is_some()
        || def.limits.timeout_s.is_some();
    entries.push(ConfigEntry::visible(
        "limits",
        serde_json::json!({
            "max_turns": def.limits.max_turns,
            "max_model_calls": def.limits.max_model_calls,
            "max_cost_micro_usd": def.limits.max_cost_micro_usd,
            "timeout_s": def.limits.timeout_s,
        }),
        manifest_or_default(any_limit),
    ));

    // Scratch + trace locations (resolved paths).
    let scratch = def
        .scratchpad
        .root
        .as_deref()
        .map(|r| resolve(base_dir, r).display().to_string());
    entries.push(ConfigEntry::visible(
        "scratchpad.root",
        scratch.clone().unwrap_or_else(|| "<store>/.scratch".to_string()),
        manifest_or_default(scratch.is_some()),
    ));
    let store = def
        .traces
        .store
        .as_deref()
        .map(|s| resolve(base_dir, s).display().to_string());
    entries.push(ConfigEntry::visible(
        "traces.store",
        store
            .clone()
            .unwrap_or_else(|| resolve(base_dir, DEFAULT_TRACE_DIRNAME).display().to_string()),
        manifest_or_default(store.is_some()),
    ));

    // Answer-extra schema (T3.4) provenance, if declared.
    if def.answer.extra_schema.is_some() {
        entries.push(ConfigEntry::visible("answer.extra_schema", true, Manifest));
    }
    if let Some(file) = &def.answer.extra_schema_file {
        entries.push(ConfigEntry::visible(
            "answer.extra_schema_file",
            resolve(base_dir, file).display().to_string(),
            Manifest,
        ));
    }

    entries
}

/// Resolve the declared `[answer]` extra schema: prefer the inline
/// `extra_schema`, else read `extra_schema_file` (relative to the definition
/// folder) as JSON. Returns `Ok(None)` when nothing is declared; a read/parse
/// failure is a non-fatal warning (the schema is advisory).
fn resolve_answer_schema(
    def: &AgentDefinition,
    base_dir: &Path,
) -> Result<Option<serde_json::Value>, String> {
    if let Some(schema) = &def.answer.extra_schema {
        return Ok(Some(schema.clone()));
    }
    if let Some(file) = &def.answer.extra_schema_file {
        let path = resolve(base_dir, file);
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("answer.extra_schema_file `{}`: {e} (ignored)", path.display()))?;
        let schema: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("answer.extra_schema_file `{}`: {e} (ignored)", path.display()))?;
        return Ok(Some(schema));
    }
    Ok(None)
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

/// Wire one `[tools.agent.<name>]` grant into an `agent_<name>` tool spec
/// (ARCHITECTURE §20.5, ROADMAP T3.8). Resolution follows §20.3 weight order:
/// a `ref` pointing at a **definition folder** (contains `hugr.toml`) runs the
/// child in-process under the interpreter; a `ref` pointing at a **file** runs
/// it as a subprocess artifact speaking the CLI JSON contract (§21.1). At zero
/// remaining depth the grant becomes an `agent_depth_exceeded` stub (cycle cut).
async fn build_agent_tool(
    grant: &ToolGrant,
    base_dir: &Path,
    agent_depth: u32,
) -> Result<AgentToolSpec, RuntimeError> {
    let tool_name = format!("agent_{}", grant.name);
    let err = |message: String| RuntimeError::Agent {
        name: grant.name.clone(),
        message,
    };

    // Depth/cycle cut: no child is built or run.
    if agent_depth == 0 {
        return Ok(AgentToolSpec::new(
            &tool_name,
            "delegation refused: max agent depth reached",
            depth_exceeded_resolver(grant.name.clone()),
        ));
    }

    let reference = grant
        .config
        .get("ref")
        .and_then(|v| v.as_str())
        .ok_or_else(|| err("missing string `ref` (definition folder or artifact)".into()))?;
    let resolved = resolve(base_dir, reference);

    if resolved.is_dir() {
        // Interpreter path: load + build the child under its own manifest.
        let child_def = AgentDefinition::load(&resolved)
            .map_err(|e| err(format!("loading child definition: {e}")))?;
        let (child, _child_warnings) = Box::pin(build_agent_depth(&child_def, agent_depth - 1))
            .await
            .map_err(|e| err(format!("building child agent: {e}")))?;
        let description = child.describe().description;
        let child = Arc::new(child);
        let resolver: AgentToolResolver = Arc::new(move |ask: Ask| {
            let child = child.clone();
            Box::pin(async move { child.ask(ask).await.map_err(|e| e.to_string()) })
        });
        Ok(AgentToolSpec::new(tool_name, description, resolver))
    } else if resolved.is_file() {
        // Subprocess-artifact path: spawn the built binary per call.
        let bin = resolved.clone();
        let resolver: AgentToolResolver = Arc::new(move |ask: Ask| {
            let bin = bin.clone();
            Box::pin(async move { run_subprocess_agent(&bin, ask).await })
        });
        Ok(AgentToolSpec::new(
            tool_name,
            format!("subagent artifact at {}", resolved.display()),
            resolver,
        ))
    } else {
        Err(err(format!(
            "`ref` does not resolve to a definition folder or artifact: {}",
            resolved.display()
        )))
    }
}

/// Run a built agent artifact as a subprocess over the CLI JSON contract
/// (§21.1): `<bin> <question> --json [--trace <id>]`, then parse the `Answer`
/// from stdout. Blob forwarding is a later refinement.
async fn run_subprocess_agent(bin: &Path, ask: Ask) -> Result<Answer, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg(&ask.question).arg("--json");
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

/// Resolve a manifest path against the definition folder (absolute paths pass
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

    #[test]
    fn effective_config_has_provenance_and_redacts_the_secret() {
        let src = r#"
[agent]
name = "policy-docs"
version = "0.2.0"
[models]
api_key_env = "HUGR_T35_TEST_KEY"
[models.medium]
model = "m"
[tools.fs_read]
root = "./policies"
[limits]
max_model_calls = 7
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let entries = effective_config(&def, Path::new("/agent"));
        let by_key = |k: &str| entries.iter().find(|e| e.key == k).expect(k);

        // Manifest-declared values are tagged Manifest.
        assert_eq!(by_key("agent.name").provenance, ConfigProvenance::Manifest);
        assert_eq!(
            by_key("agent.version").provenance,
            ConfigProvenance::Manifest
        );
        assert_eq!(by_key("limits").provenance, ConfigProvenance::Manifest);
        assert_eq!(by_key("tools.fs_read").provenance, ConfigProvenance::Manifest);

        // A value that falls back is Default (no description declared).
        assert_eq!(
            by_key("agent.description").provenance,
            ConfigProvenance::Default
        );

        // The env var *name* is surfaced (Manifest), but the secret itself is
        // redacted and env-provenanced — the key value must never appear.
        assert_eq!(
            by_key("models.api_key_env").value,
            serde_json::json!("HUGR_T35_TEST_KEY")
        );
        let secret = by_key("models.api_key");
        assert!(secret.redacted, "the provider key must be redacted");
        assert_eq!(secret.provenance, ConfigProvenance::Env);
        assert_ne!(secret.value, serde_json::json!("HUGR_T35_TEST_KEY"));
        // No entry anywhere leaks a real key value (there is none set here).
        for e in &entries {
            assert_ne!(e.value.as_str(), Some("super-secret-value"));
        }
    }

    #[tokio::test]
    async fn group_bound_tool_is_not_eagerly_registered() {
        // A tool bound to a resource group (§18.5, T3.7) is registered only
        // per-ask when a grant arrives — so it is absent from the agent card,
        // which reflects the always-on capabilities. Only the scratch tools show.
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"
[tools.fs_read]
root = "group:policies"
"#;
        let mut def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        let (agent, warnings) = build_agent(&def).await.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let tool_names: Vec<_> = agent
            .describe()
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect();
        // The group-bound fs_read family is provably absent without a grant.
        assert!(!tool_names.iter().any(|n| n.starts_with("fs_")), "{tool_names:?}");
        // The always-present scratch tools are still there.
        assert!(tool_names.contains(&"scratch_read".to_string()), "{tool_names:?}");
    }

    /// Write a minimal definition folder and return its path.
    fn write_def(tag: &str, toml: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("hugr-agtool-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hugr.toml"), toml).unwrap();
        dir
    }

    #[tokio::test]
    async fn agent_as_tool_interpreter_grant_registers_agent_tool() {
        // A child definition folder granted to a parent (§20.5, T3.8) registers
        // an `agent_<name>` capability on the parent's card.
        let child_dir = write_def(
            "child",
            "[agent]\nname = \"child\"\ndescription = \"answers sub-questions\"\n[models.medium]\nmodel = \"m\"\n",
        );
        let parent_src = format!(
            "[agent]\nname = \"parent\"\n[models.medium]\nmodel = \"m\"\n[tools.agent.helper]\nref = {:?}\n",
            child_dir.display().to_string()
        );
        let mut def = AgentDefinition::parse(&parent_src, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        let (agent, warnings) = build_agent(&def).await.unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<_> = agent.describe().tools.iter().map(|t| t.name.clone()).collect();
        assert!(names.contains(&"agent_helper".to_string()), "{names:?}");
        let _ = std::fs::remove_dir_all(&child_dir);
    }

    #[tokio::test]
    async fn agent_as_tool_cycle_terminates_via_depth_cap() {
        // a → b → a → … must build (not hang / overflow): the depth cap turns
        // the deepest grant into a stub. We just require build() to complete.
        let a_dir = write_def("cyc-a", "");
        let b_dir = write_def("cyc-b", "");
        std::fs::write(
            a_dir.join("hugr.toml"),
            format!(
                "[agent]\nname = \"a\"\n[models.medium]\nmodel = \"m\"\n[tools.agent.b]\nref = {:?}\n",
                b_dir.display().to_string()
            ),
        )
        .unwrap();
        std::fs::write(
            b_dir.join("hugr.toml"),
            format!(
                "[agent]\nname = \"b\"\n[models.medium]\nmodel = \"m\"\n[tools.agent.a]\nref = {:?}\n",
                a_dir.display().to_string()
            ),
        )
        .unwrap();
        let def = AgentDefinition::load(&a_dir).unwrap();
        let (agent, _warnings) = build_agent(&def).await.unwrap();
        assert!(
            agent.describe().tools.iter().any(|t| t.name == "agent_b"),
            "the top-level grant is still registered"
        );
        let _ = std::fs::remove_dir_all(&a_dir);
        let _ = std::fs::remove_dir_all(&b_dir);
    }

    #[tokio::test]
    async fn agent_as_tool_bad_ref_is_a_build_error() {
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"
[tools.agent.receipts]
ref = "./does-not-exist"
"#;
        let mut def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        def.source_dir = Some(std::env::temp_dir());
        let err = build_agent(&def).await.err().expect("bad ref → build error");
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
