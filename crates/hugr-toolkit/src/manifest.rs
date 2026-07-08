//! The `hugr.toml` manifest (ARCHITECTURE §20.1, ROADMAP T1.1).
//!
//! A subagent definition is an auditable *folder*, not a Rust project: a
//! `hugr.toml` manifest plus a `SYSTEM.md` system prompt beside it. This module
//! parses that folder into a typed [`AgentDefinition`].
//!
//! Unknown keys are **hard errors** (`deny_unknown_fields` on every
//! fixed-schema section, plus a top-level check) — a typo in a manifest fails
//! the parse instead of silently doing nothing. Tier names under `[models]`
//! and scope keys under `[tools.<name>]` are caller-defined, so they are never
//! flagged.
//!
//! The typed shape mirrors the pieces an agent runtime declares (system prompt,
//! model tiers + pricing, granted tools, limits); T1.3 (`hugr run`) assembles a
//! `hugr-agent` runtime from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The manifest file name expected inside a definition folder.
pub const MANIFEST_FILE: &str = "hugr.toml";
/// The system-prompt file name expected inside a definition folder.
pub const SYSTEM_PROMPT_FILE: &str = "SYSTEM.md";

/// Reserved keys under `[tools]` that namespace *external* tool grants
/// (§20.3). Every other key under `[tools]` is a predefined-library grant.
const TOOL_NAMESPACES: &[(&str, ToolKind)] = &[("mcp", ToolKind::Mcp), ("agent", ToolKind::Agent)];

/// A parsed subagent definition (ARCHITECTURE §20). Produced by
/// [`AgentDefinition::load`] (a folder) or [`AgentDefinition::parse`] (a manifest
/// string). Every optional section carries defaults, so a minimal manifest —
/// `[agent]` + one `[models.<tier>]` — parses.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct AgentDefinition {
    /// Identity block (`[agent]`): name (required), version, description.
    pub agent: AgentMeta,
    /// Model tiers + provider knobs (`[models]`).
    pub models: ModelsConfig,
    /// Granted tools (`[tools.*]`), deterministically ordered.
    pub tools: Vec<ToolGrant>,
    /// Declared runtime limits (`[limits]`).
    pub limits: LimitsConfig,
    /// Scratchpad configuration (`[scratchpad]`).
    pub scratchpad: ScratchpadConfig,
    /// Trace-store configuration (`[traces]`).
    pub traces: TracesConfig,
    /// The `SYSTEM.md` system prompt, if present beside the manifest.
    pub system_prompt: Option<String>,
    /// The folder the definition was loaded from ([`AgentDefinition::load`]).
    pub source_dir: Option<PathBuf>,
    /// An explicit provider API key supplied by an embedding host (e.g. the
    /// docs Python binding), taking precedence over `[models].api_key_env`.
    /// In-memory only — never parsed from or written to a manifest.
    pub provider_api_key: Option<String>,
}

/// The `[agent]` identity block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct AgentMeta {
    // `default` so a missing name surfaces our located "name is required"
    // diagnostic rather than serde's generic "missing field" error.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
}

/// The `[models]` block: shared provider settings plus one nested table per
/// logical tier (`[models.small]`, `[models.medium]`, `[models.big]`, §5.3).
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub struct ModelsConfig {
    /// Provider base URL shared by every tier (`base_url`).
    pub base_url: Option<String>,
    /// Environment variable holding the provider API key (`api_key_env`) —
    /// the value itself is never stored in the manifest.
    pub api_key_env: Option<String>,
    /// Which tier the turn policy calls by default (`default`); when unset the
    /// runtime falls back to `medium`, else the first tier.
    pub default: Option<String>,
    /// Logical tier → model id + pricing + sampling knobs.
    pub tiers: BTreeMap<String, TierConfig>,
}

/// One `[models.<tier>]` entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TierConfig {
    /// Provider model id (required per tier).
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_usd_per_m_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_usd_per_m_tokens: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

/// A single granted tool (`[tools.<name>]` or `[tools.<ns>.<instance>]`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct ToolGrant {
    /// Tool name — a library tool id (`fs_read`) or, for namespaced grants, the
    /// instance name (`docs` in `[tools.mcp.docs]`).
    pub name: String,
    /// Which extension path this grant came from (§20.3).
    pub kind: ToolKind,
    /// Scope / configuration parameters, verbatim. Keys are tool-specific, so
    /// they are never unknown-key-checked here.
    pub config: serde_json::Value,
}

/// How a granted tool is provided (ARCHITECTURE §20.3), in order of weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ToolKind {
    /// A vetted predefined-library capability (`[tools.fs_read]`, §20.2).
    Library,
    /// A stdio MCP server's namespaced tools (`[tools.mcp.<name>]`).
    Mcp,
    /// Another Hugr agent granted as a tool (`[tools.agent.<name>]`, §20.5).
    Agent,
}

/// The `[limits]` block. Enforcement is ROADMAP T3.1; T1.1 only parses it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct LimitsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

/// The `[scratchpad]` block (ARCHITECTURE §19.3).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct ScratchpadConfig {
    /// Override the per-lineage scratch root; defaults to a hidden subtree of
    /// the trace store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// The `[traces]` block (ARCHITECTURE §19.1).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct TracesConfig {
    /// Directory the immutable trace store lives in; defaults per surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

/// Failure to load or parse a definition. Run failures are *answers* (§18.1);
/// these are strictly load-time problems.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// No `hugr.toml` in the folder.
    #[error("no {MANIFEST_FILE} found in {dir}")]
    NotFound { dir: PathBuf },
    /// Reading a definition file failed.
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The manifest is not valid TOML (the message carries toml's own
    /// line/column rendering).
    #[error("{path}: {message}")]
    Parse { path: PathBuf, message: String },
    /// The manifest is valid TOML but semantically incomplete/invalid — a
    /// missing required key, an unknown key, a dangling default tier.
    #[error("{path}: {message}")]
    Validate { path: PathBuf, message: String },
}

impl AgentDefinition {
    /// Load a definition folder: `<dir>/hugr.toml` (required) plus
    /// `<dir>/SYSTEM.md` (optional). The returned definition records
    /// `source_dir` so relative tool scopes can later be resolved against it.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let dir = dir.as_ref();
        let manifest_path = dir.join(MANIFEST_FILE);
        let src = match std::fs::read_to_string(&manifest_path) {
            Ok(src) => src,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(ManifestError::NotFound {
                    dir: dir.to_path_buf(),
                });
            }
            Err(source) => {
                return Err(ManifestError::Io {
                    path: manifest_path,
                    source,
                });
            }
        };

        let mut def = Self::parse(&src, &manifest_path)?;

        let prompt_path = dir.join(SYSTEM_PROMPT_FILE);
        match std::fs::read_to_string(&prompt_path) {
            Ok(prompt) => def.system_prompt = Some(prompt),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(ManifestError::Io {
                    path: prompt_path,
                    source,
                });
            }
        }

        def.source_dir = Some(dir.to_path_buf());
        Ok(def)
    }

    /// Parse a manifest string. `path` is only used to label diagnostics.
    pub fn parse(src: &str, path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let path = path.as_ref();
        let table: toml::Table = toml::from_str(src).map_err(|err| ManifestError::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;

        reject_unknown_top_level(&table, path)?;
        let agent = parse_agent(&table, path)?;
        let models = parse_models(&table, path)?;
        let tools = parse_tools(&table, path)?;
        let limits: LimitsConfig = parse_section(&table, "limits", path)?;
        let scratchpad: ScratchpadConfig = parse_section(&table, "scratchpad", path)?;
        let traces: TracesConfig = parse_section(&table, "traces", path)?;

        Ok(Self {
            agent,
            models,
            tools,
            limits,
            scratchpad,
            traces,
            system_prompt: None,
            source_dir: None,
            provider_api_key: None,
        })
    }

    /// The default tier selector: the explicit `[models].default`, else
    /// `medium` if present, else the first tier by name, else `None`.
    pub fn default_tier(&self) -> Option<&str> {
        if let Some(default) = &self.models.default {
            return Some(default.as_str());
        }
        if self.models.tiers.contains_key("medium") {
            return Some("medium");
        }
        self.models.tiers.keys().next().map(String::as_str)
    }
}

fn parse_agent(table: &toml::Table, path: &Path) -> Result<AgentMeta, ManifestError> {
    let Some(value) = table.get("agent") else {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "missing required [agent] section".to_string(),
        });
    };
    let agent: AgentMeta =
        value
            .clone()
            .try_into()
            .map_err(|err: toml::de::Error| ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("[agent]: {}", err.message()),
            })?;
    if agent.name.trim().is_empty() {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "[agent].name is required and must not be empty".to_string(),
        });
    }
    Ok(agent)
}

fn parse_models(table: &toml::Table, path: &Path) -> Result<ModelsConfig, ManifestError> {
    let Some(value) = table.get("models").and_then(toml::Value::as_table) else {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "missing required [models] section with at least one tier".to_string(),
        });
    };

    let mut models = ModelsConfig {
        base_url: value
            .get("base_url")
            .and_then(|v| v.as_str())
            .map(String::from),
        api_key_env: value
            .get("api_key_env")
            .and_then(|v| v.as_str())
            .map(String::from),
        default: value
            .get("default")
            .and_then(|v| v.as_str())
            .map(String::from),
        tiers: BTreeMap::new(),
    };

    // Every non-reserved key under [models] is a tier table.
    for (key, tier_value) in value {
        if matches!(key.as_str(), "base_url" | "api_key_env" | "default") {
            continue;
        }
        let tier: TierConfig = tier_value
            .clone()
            .try_into()
            .map_err(|err: toml::de::Error| ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("[models.{key}]: {}", err.message()),
            })?;
        if tier.model.trim().is_empty() {
            return Err(ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("[models.{key}].model is required"),
            });
        }
        models.tiers.insert(key.clone(), tier);
    }

    if models.tiers.is_empty() {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "[models] must declare at least one tier, e.g. [models.medium]".to_string(),
        });
    }
    if let Some(default) = &models.default
        && !models.tiers.contains_key(default)
    {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[models].default = \"{default}\" names no declared tier"),
        });
    }

    Ok(models)
}

fn parse_tools(table: &toml::Table, path: &Path) -> Result<Vec<ToolGrant>, ManifestError> {
    let Some(tools_table) = table.get("tools").and_then(toml::Value::as_table) else {
        return Ok(Vec::new());
    };

    let mut grants = Vec::new();
    for (key, value) in tools_table {
        if let Some((_, kind)) = TOOL_NAMESPACES.iter().find(|(ns, _)| ns == key) {
            // Namespaced external grant: each subtable is one instance.
            let Some(instances) = value.as_table() else {
                return Err(ManifestError::Validate {
                    path: path.to_path_buf(),
                    message: format!("[tools.{key}] must be a table of named instances"),
                });
            };
            for (instance, cfg) in instances {
                grants.push(ToolGrant {
                    name: instance.clone(),
                    kind: *kind,
                    config: toml_to_json(cfg),
                });
            }
        } else {
            grants.push(ToolGrant {
                name: key.clone(),
                kind: ToolKind::Library,
                config: toml_to_json(value),
            });
        }
    }
    grants.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(grants)
}

/// Parse a fixed-schema optional section into `T` — unknown keys are hard
/// errors via each section type's `deny_unknown_fields`.
fn parse_section<T>(table: &toml::Table, section: &str, path: &Path) -> Result<T, ManifestError>
where
    T: Default + serde::de::DeserializeOwned,
{
    let Some(value) = table.get(section) else {
        return Ok(T::default());
    };
    value
        .clone()
        .try_into()
        .map_err(|err: toml::de::Error| ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[{section}]: {}", err.message()),
        })
}

/// An unrecognized top-level section is a hard error, matching the
/// `deny_unknown_fields` posture of the fixed-schema sections.
fn reject_unknown_top_level(table: &toml::Table, path: &Path) -> Result<(), ManifestError> {
    const KNOWN: &[&str] = &["agent", "models", "tools", "limits", "scratchpad", "traces"];
    for key in table.keys() {
        if !KNOWN.contains(&key.as_str()) {
            return Err(ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("unknown top-level section `[{key}]`"),
            });
        }
    }
    Ok(())
}

/// Convert a parsed TOML value into `serde_json::Value` for downstream
/// (`hugr-agent`) consumption — the rest of the stack speaks JSON `Value`.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[agent]
name = "policy-docs"

[models.medium]
model = "google/gemma-4-31B-it"
"#;

    #[test]
    fn minimal_manifest_parses() {
        let def = AgentDefinition::parse(MINIMAL, "hugr.toml").unwrap();
        assert_eq!(def.agent.name, "policy-docs");
        assert_eq!(def.default_tier(), Some("medium"));
        assert!(def.tools.is_empty());
    }

    #[test]
    fn missing_name_is_a_validation_error() {
        let src = "[agent]\nversion = \"0.1.0\"\n[models.medium]\nmodel = \"m\"\n";
        let err = AgentDefinition::parse(src, "hugr.toml").unwrap_err();
        assert!(matches!(err, ManifestError::Validate { .. }));
        assert!(err.to_string().contains("name is required"));
    }

    #[test]
    fn missing_models_is_a_validation_error() {
        let src = "[agent]\nname = \"x\"\n";
        let err = AgentDefinition::parse(src, "hugr.toml").unwrap_err();
        assert!(matches!(err, ManifestError::Validate { .. }));
        assert!(err.to_string().contains("[models]"));
    }

    #[test]
    fn syntax_error_carries_line_and_column() {
        let src = "[agent]\nname = \n";
        let err = AgentDefinition::parse(src, "hugr.toml").unwrap_err();
        assert!(matches!(err, ManifestError::Parse { .. }));
        assert!(err.to_string().contains("line 2"), "{err}");
    }

    #[test]
    fn unknown_keys_are_hard_errors() {
        let base = "[agent]\nname = \"x\"\n[models.medium]\nmodel = \"m\"\n";
        for (src, needle) in [
            (
                format!("{base}[limits]\nmaxturns = 6\n"),
                "maxturns".to_string(),
            ),
            (
                "[agent]\nname = \"x\"\ndescriptn = \"typo\"\n[models.medium]\nmodel = \"m\"\n"
                    .to_string(),
                "descriptn".to_string(),
            ),
            (
                format!("{base}[bogus]\nwhatever = true\n"),
                "bogus".to_string(),
            ),
            (
                format!("{base}top_p = 0.9\n"),
                "top_p".to_string(), // deleted knob: now unknown
            ),
        ] {
            let err = AgentDefinition::parse(&src, "hugr.toml").unwrap_err();
            assert!(matches!(err, ManifestError::Validate { .. }), "{src}");
            assert!(err.to_string().contains(&needle), "{err}");
        }
    }

    #[test]
    fn tiers_and_pricing_parse() {
        let src = r#"
[agent]
name = "x"

[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "X_API_KEY"
default = "big"

[models.small]
model = "small-m"

[models.big]
model = "big-m"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5
temperature = 0.2
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        assert_eq!(
            def.models.base_url.as_deref(),
            Some("https://router.huggingface.co/v1")
        );
        assert_eq!(def.models.api_key_env.as_deref(), Some("X_API_KEY"));
        assert_eq!(def.default_tier(), Some("big"));
        assert_eq!(def.models.tiers.len(), 2);
        let big = &def.models.tiers["big"];
        assert_eq!(big.model, "big-m");
        assert_eq!(big.input_usd_per_m_tokens, Some(1.0));
        assert_eq!(big.temperature, Some(0.2));
    }

    #[test]
    fn default_tier_naming_a_missing_tier_is_rejected() {
        let src = "[agent]\nname=\"x\"\n[models]\ndefault=\"nope\"\n[models.small]\nmodel=\"m\"\n";
        let err = AgentDefinition::parse(src, "hugr.toml").unwrap_err();
        assert!(err.to_string().contains("names no declared tier"), "{err}");
    }

    #[test]
    fn library_and_namespaced_tools_parse() {
        let src = r#"
[agent]
name = "x"
[models.medium]
model = "m"

[tools.fs_read]
root = "./policies"

[tools.sqlite_query]
file = "./expenses.db"

[tools.mcp.docs]
command = "docs-mcp"
args = ["--stdio"]

[tools.agent.receipts]
artifact = "./receipts"
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        assert_eq!(def.tools.len(), 4);
        // Library tools sort before namespaced ones (ToolKind ordering).
        assert_eq!(def.tools[0].kind, ToolKind::Library);
        assert_eq!(def.tools[0].name, "fs_read");
        assert_eq!(def.tools[0].config["root"], "./policies");
        let mcp = def.tools.iter().find(|t| t.kind == ToolKind::Mcp).unwrap();
        assert_eq!(mcp.name, "docs");
        assert_eq!(mcp.config["command"], "docs-mcp");
        let agent = def
            .tools
            .iter()
            .find(|t| t.kind == ToolKind::Agent)
            .unwrap();
        assert_eq!(agent.name, "receipts");
    }
}
