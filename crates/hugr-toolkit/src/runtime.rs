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

use hugr_agent::{Agent, AgentLimits, Pricing, TraceStore};
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
}

/// Assemble a [`Agent`] from a definition, collecting non-fatal build warnings
/// (e.g. an external-tool grant that is not yet wired). Relative scopes resolve
/// against the definition's `source_dir` (else the process cwd).
pub async fn build_agent(def: &AgentDefinition) -> Result<(Agent, Vec<String>), RuntimeError> {
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
                for capability in tools::build_library_grant(grant, &base_dir)? {
                    builder = builder.capability(capability);
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
                warnings.push(format!(
                    "agent-as-tool grant `{}` is not wired yet (ROADMAP T3.8); skipped",
                    grant.name
                ));
            }
        }
    }

    // Declared limits (enforcement: T3.1; recorded for the audit surface now).
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

    Ok((builder.build(), warnings))
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

    #[tokio::test]
    async fn agent_as_tool_grant_warns_and_is_skipped() {
        // Agent-as-tool is deferred to T3.8; it warns rather than connecting.
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"
[tools.agent.receipts]
ref = "receipts"
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        let (_agent, warnings) = build_agent(&def).await.unwrap();
        assert!(
            warnings.iter().any(|w| w.contains("agent-as-tool grant")),
            "{warnings:?}"
        );
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
