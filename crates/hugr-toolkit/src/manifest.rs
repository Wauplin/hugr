//! The `hugr.toml` manifest.
//!
//! A subagent definition is an auditable Rust crate folder: a `Cargo.toml`, a
//! `hugr.toml` manifest, and a `SYSTEM.md` system prompt beside it. This module
//! parses that folder into a typed [`AgentDefinition`].
//!
//! Unknown keys are **hard errors** (`deny_unknown_fields` on every
//! fixed-schema section, plus a top-level check) — a typo in a manifest fails
//! the parse instead of silently doing nothing. Tier names under `[models]`
//! and scope keys under `[tools.<name>]` are caller-defined, so they are never
//! flagged.
//!
//! The typed shape mirrors the pieces an agent runtime declares (system prompt,
//! model tiers + pricing, granted tools, limits, runtime arguments, response
//! contract); `hugr run` assembles a `hugr-agent` runtime from it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use croner::Cron;
use serde::{Deserialize, Serialize};

/// The manifest file name expected inside an agent crate folder.
pub const MANIFEST_FILE: &str = "hugr.toml";
/// The Cargo manifest expected beside `hugr.toml` in an agent crate folder.
pub const CARGO_MANIFEST_FILE: &str = "Cargo.toml";
/// The system-prompt file name expected inside an agent crate folder.
pub const SYSTEM_PROMPT_FILE: &str = "SYSTEM.md";

/// Reserved keys under `[tools]` that namespace external tool grants. Every
/// other key under `[tools]` is a predefined-library grant.
const TOOL_NAMESPACES: &[(&str, ToolKind)] = &[("mcp", ToolKind::Mcp), ("agent", ToolKind::Agent)];

/// A parsed subagent definition. Produced by [`AgentDefinition::load`] (a
/// folder) or [`AgentDefinition::parse`] (a manifest string). Every optional
/// section carries defaults, so a minimal manifest parses.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentDefinition {
    /// Identity block (`[agent]`): name (required), version, description.
    pub agent: AgentMeta,
    /// Model tiers + provider knobs (`[models]`).
    pub models: ModelsConfig,
    /// Granted tools (`[tools.*]`), deterministically ordered.
    pub tools: Vec<ToolGrant>,
    /// Standard Agent Skills folders bundled with this definition. Paths are
    /// resolved relative to the agent crate.
    pub skills: Vec<String>,
    /// Declared runtime limits (`[limits]`).
    pub limits: LimitsConfig,
    /// Recurring asks (`[cron.<name>]`), deterministically ordered.
    pub cron: Vec<CronJobConfig>,
    /// Context projection and deterministic compaction (`[context]`).
    pub context: ContextConfig,
    /// Scratchpad configuration (`[scratchpad]`).
    pub scratchpad: ScratchpadConfig,
    /// Trace-store configuration (`[traces]`).
    pub traces: TracesConfig,
    /// Runtime arguments whose values patch the manifest for one invocation.
    pub runtime: RuntimeConfig,
    /// Optional manifest-owned response contract (`[response]`).
    pub response: ResponseConfig,
    /// JSON Schema loaded from a schema file. Rust response types are discovered
    /// from the agent crate by the generated shim.
    pub response_schema: Option<serde_json::Value>,
    /// The `SYSTEM.md` system prompt, if present beside the manifest.
    pub system_prompt: Option<String>,
    /// The folder the definition was loaded from ([`AgentDefinition::load`]).
    pub source_dir: Option<PathBuf>,
}

/// The `[agent]` identity block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
/// logical tier (`[models.small]`, `[models.medium]`, `[models.big]`).
#[derive(Clone, Debug, Default, PartialEq)]
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
pub struct ToolGrant {
    /// Tool name — a library tool id (`fs_read`) or, for namespaced grants, the
    /// instance name (`docs` in `[tools.mcp.docs]`).
    pub name: String,
    /// Which extension path this grant came from.
    pub kind: ToolKind,
    /// Scope / configuration parameters, verbatim. Keys are tool-specific, so
    /// they are never unknown-key-checked here.
    pub config: serde_json::Value,
}

/// How a granted tool is provided, in order of weight.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    /// A vetted predefined-library capability (`[tools.fs_read]`).
    Library,
    /// A stdio MCP server's namespaced tools (`[tools.mcp.<name>]`).
    Mcp,
    /// Another Hugr agent granted as a tool (`[tools.agent.<name>]`).
    Agent,
}

/// The `[limits]` block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LimitsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_s: Option<u64>,
}

/// One `[cron.<name>]` recurring ask.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CronJobConfig {
    /// Stable job id, taken from `[cron.<name>]`.
    #[serde(skip)]
    pub name: String,
    /// Five-field cron expression: minute hour day-of-month month day-of-week.
    pub schedule: String,
    /// Question to ask when the job fires.
    pub question: String,
    /// `fresh` starts every run without a parent; `chain` resumes from the previous successful run.
    #[serde(default = "default_cron_lineage")]
    pub lineage: String,
    /// Optional limits that override `[limits]` for this unattended ask.
    #[serde(default)]
    pub limits: LimitsConfig,
}

fn default_cron_lineage() -> String {
    "fresh".to_string()
}

/// The `[context]` block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_recent_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_block_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary_model: Option<String>,
    #[serde(default)]
    pub forget: ContextForgetConfig,
}

/// Deterministic tool-result forget rules under `[context.forget]`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextForgetConfig {
    #[serde(default)]
    pub tool_ttl: BTreeMap<String, u32>,
    #[serde(default)]
    pub keep_last_per_tool: BTreeMap<String, u32>,
}

/// The `[scratchpad]` block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScratchpadConfig {
    /// Override the per-lineage scratch root; defaults to a hidden subtree of
    /// the trace store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
}

/// The `[traces]` block.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TracesConfig {
    /// Directory the immutable trace store lives in; defaults per surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

/// Runtime invocation arguments declared by the definition.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RuntimeConfig {
    /// Arguments, deterministically ordered by manifest key.
    pub args: Vec<RuntimeArg>,
}

/// One runtime argument. It is surfaced as a CLI argument and an MCP `ask`
/// argument; its value is copied to `target` before the agent is assembled.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArg {
    /// Stable argument id, taken from `[runtime.args.<name>]`.
    #[serde(skip)]
    pub name: String,
    /// Manifest path to patch, e.g. `tools.fs_read.root`.
    pub target: String,
    /// Help text for generated surfaces.
    #[serde(default)]
    pub help: String,
    /// Expose as a positional before `question` instead of as `--<name>`.
    #[serde(default)]
    pub positional: bool,
    /// Whether the ask path requires a value.
    #[serde(default)]
    pub required: bool,
    /// Optional environment fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// Optional default fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// Optional long flag name. Defaults to `name` with `_` replaced by `-`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
}

/// Optional manifest-owned response-shape config. A Rust response contract is
/// not named here: agent crates expose `RESPONSE_RUST_TYPE`, and generated
/// shims link that crate to derive JSON Schema from the type.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseConfig {
    /// Legacy path to a JSON Schema file, relative to the agent crate folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
}

/// Failure to load or parse a definition. Run failures are *answers*;
/// these are strictly load-time problems.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// No `hugr.toml` in the folder.
    #[error("no {MANIFEST_FILE} found in {dir}")]
    NotFound { dir: PathBuf },
    /// No `Cargo.toml` beside `hugr.toml`.
    #[error("no {CARGO_MANIFEST_FILE} found in agent crate folder {dir}")]
    NotRustCrate { dir: PathBuf },
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
    /// Load an agent crate folder: `<dir>/Cargo.toml` and `<dir>/hugr.toml`
    /// (required) plus `<dir>/SYSTEM.md` (optional). The returned definition records
    /// `source_dir` so relative tool scopes can later be resolved against it.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let dir = dir.as_ref();
        if !dir.join(CARGO_MANIFEST_FILE).is_file() {
            return Err(ManifestError::NotRustCrate {
                dir: dir.to_path_buf(),
            });
        }
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

        if let Some(schema) = &def.response.schema {
            let schema_path = resolve_def_path(dir, schema);
            let schema_src =
                std::fs::read_to_string(&schema_path).map_err(|source| ManifestError::Io {
                    path: schema_path.clone(),
                    source,
                })?;
            let schema_json =
                serde_json::from_str(&schema_src).map_err(|err| ManifestError::Validate {
                    path: schema_path,
                    message: format!("response schema is not valid JSON: {err}"),
                })?;
            def.response_schema = Some(schema_json);
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
        let skills = table
            .get("skills")
            .map(|value| value.clone().try_into())
            .transpose()
            .map_err(|err: toml::de::Error| ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!(
                    "`skills` must be an array of folder paths: {}",
                    err.message()
                ),
            })?
            .unwrap_or_default();
        let limits: LimitsConfig = parse_section(&table, "limits", path)?;
        let cron = parse_cron(&table, path)?;
        let context: ContextConfig = parse_section(&table, "context", path)?;
        validate_context(&context, path)?;
        let scratchpad: ScratchpadConfig = parse_section(&table, "scratchpad", path)?;
        let traces: TracesConfig = parse_section(&table, "traces", path)?;
        let runtime = parse_runtime(&table, path)?;
        let response: ResponseConfig = parse_section(&table, "response", path)?;

        Ok(Self {
            agent,
            models,
            tools,
            skills,
            limits,
            cron,
            context,
            scratchpad,
            traces,
            runtime,
            response,
            response_schema: None,
            system_prompt: None,
            source_dir: None,
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

fn parse_cron(table: &toml::Table, path: &Path) -> Result<Vec<CronJobConfig>, ManifestError> {
    let Some(value) = table.get("cron") else {
        return Ok(Vec::new());
    };
    let cron_table = value.as_table().ok_or_else(|| ManifestError::Validate {
        path: path.to_path_buf(),
        message: "[cron] must be a table of named jobs".to_string(),
    })?;
    let mut jobs = Vec::new();
    for (name, value) in cron_table {
        validate_cron_name(name, path)?;
        let mut job: CronJobConfig =
            value
                .clone()
                .try_into()
                .map_err(|err: toml::de::Error| ManifestError::Validate {
                    path: path.to_path_buf(),
                    message: format!("[cron.{name}]: {}", err.message()),
                })?;
        job.name = name.clone();
        validate_cron_job(&job, path)?;
        jobs.push(job);
    }
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(jobs)
}

fn validate_cron_name(name: &str, path: &Path) -> Result<(), ManifestError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!(
                "[cron.{name}] names must contain only ASCII letters, digits, `_`, or `-`"
            ),
        })
    }
}

fn validate_cron_job(job: &CronJobConfig, path: &Path) -> Result<(), ManifestError> {
    if job.schedule.split_whitespace().count() != 5 {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!(
                "[cron.{}].schedule must be a five-field cron expression",
                job.name
            ),
        });
    }
    Cron::new(&job.schedule)
        .with_seconds_optional()
        .parse()
        .map_err(|err| ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[cron.{}].schedule is invalid: {err}", job.name),
        })?;
    if job.question.trim().is_empty() {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[cron.{}].question is required", job.name),
        });
    }
    if !matches!(job.lineage.as_str(), "fresh" | "chain") {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[cron.{}].lineage must be \"fresh\" or \"chain\"", job.name),
        });
    }
    Ok(())
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

fn resolve_def_path(base: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
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

fn parse_runtime(table: &toml::Table, path: &Path) -> Result<RuntimeConfig, ManifestError> {
    let Some(value) = table.get("runtime") else {
        return Ok(RuntimeConfig::default());
    };
    let runtime = value.as_table().ok_or_else(|| ManifestError::Validate {
        path: path.to_path_buf(),
        message: "[runtime] must be a table".to_string(),
    })?;
    for key in runtime.keys() {
        if key != "args" {
            return Err(ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("unknown [runtime] key `{key}`"),
            });
        }
    }
    let Some(args_value) = runtime.get("args") else {
        return Ok(RuntimeConfig::default());
    };
    let args_table = args_value
        .as_table()
        .ok_or_else(|| ManifestError::Validate {
            path: path.to_path_buf(),
            message: "[runtime.args] must be a table".to_string(),
        })?;
    let mut args = Vec::new();
    for (name, value) in args_table {
        validate_runtime_name(name, path)?;
        let mut arg: RuntimeArg =
            value
                .clone()
                .try_into()
                .map_err(|err: toml::de::Error| ManifestError::Validate {
                    path: path.to_path_buf(),
                    message: format!("[runtime.args.{name}]: {}", err.message()),
                })?;
        arg.name = name.clone();
        validate_runtime_arg(&arg, path)?;
        args.push(arg);
    }
    args.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(RuntimeConfig { args })
}

fn validate_runtime_name(name: &str, path: &Path) -> Result<(), ManifestError> {
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!(
                "[runtime.args.{name}] names must contain only ASCII letters, digits, `_`, or `-`"
            ),
        })
    }
}

fn validate_runtime_arg(arg: &RuntimeArg, path: &Path) -> Result<(), ManifestError> {
    if arg.target.trim().is_empty() {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!("[runtime.args.{}].target is required", arg.name),
        });
    }
    if matches!(
        arg.name.as_str(),
        "question"
            | "trace"
            | "json"
            | "pretty"
            | "blob"
            | "describe"
            | "config"
            | "traces"
            | "stats"
            | "feedback"
            | "feedback-payload"
            | "mcp-serve"
            | "cron-serve"
            | "allow-uncapped"
    ) {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!(
                "[runtime.args.{}] conflicts with a built-in surface argument",
                arg.name
            ),
        });
    }
    if let Some(flag) = &arg.flag
        && (flag.is_empty() || flag.starts_with('-'))
    {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: format!(
                "[runtime.args.{}].flag must not be empty or start with `-`",
                arg.name
            ),
        });
    }
    Ok(())
}

fn validate_context(context: &ContextConfig, path: &Path) -> Result<(), ManifestError> {
    let compaction = context.compaction.as_deref().unwrap_or("none");
    if !matches!(compaction, "none" | "truncate" | "summarize") {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "[context].compaction must be \"none\", \"truncate\", or \"summarize\""
                .to_string(),
        });
    }
    for (key, value) in [
        ("budget_tokens", context.budget_tokens),
        ("trigger_tokens", context.trigger_tokens),
        ("max_block_tokens", context.max_block_tokens),
    ] {
        if matches!(value, Some(0)) {
            return Err(ManifestError::Validate {
                path: path.to_path_buf(),
                message: format!("[context].{key} must be greater than zero"),
            });
        }
    }
    for (table, map) in [
        ("tool_ttl", &context.forget.tool_ttl),
        ("keep_last_per_tool", &context.forget.keep_last_per_tool),
    ] {
        for (name, value) in map {
            if name.trim().is_empty() || *value == 0 {
                return Err(ManifestError::Validate {
                    path: path.to_path_buf(),
                    message: format!(
                        "[context.forget.{table}] entries need non-empty names and positive values"
                    ),
                });
            }
        }
    }
    if matches!(context.summary_model.as_deref(), Some("")) {
        return Err(ManifestError::Validate {
            path: path.to_path_buf(),
            message: "[context].summary_model must not be empty".to_string(),
        });
    }
    Ok(())
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
    const KNOWN: &[&str] = &[
        "agent",
        "models",
        "tools",
        "skills",
        "limits",
        "cron",
        "context",
        "scratchpad",
        "traces",
        "runtime",
        "response",
    ];
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
        assert_eq!(def.context, ContextConfig::default());
    }

    #[test]
    fn context_section_parses() {
        let src = r#"
[agent]
name = "x"

[models.medium]
model = "m"

[context]
budget_tokens = 4096
compaction = "summarize"
trigger_tokens = 3500
keep_recent_tokens = 500
max_block_tokens = 200
summary_model = "small"

[context.forget.tool_ttl]
page_snapshot = 2

[context.forget.keep_last_per_tool]
browser_observation = 1
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        assert_eq!(def.context.budget_tokens, Some(4096));
        assert_eq!(def.context.compaction.as_deref(), Some("summarize"));
        assert_eq!(def.context.summary_model.as_deref(), Some("small"));
        assert_eq!(def.context.forget.tool_ttl["page_snapshot"], 2);
        assert_eq!(
            def.context.forget.keep_last_per_tool["browser_observation"],
            1
        );
    }

    #[test]
    fn invalid_context_is_a_validation_error() {
        let base = "[agent]\nname = \"x\"\n[models.medium]\nmodel = \"m\"\n";
        for (src, needle) in [
            (
                format!("{base}[context]\ncompaction = \"compactify\"\n"),
                "compaction".to_string(),
            ),
            (
                format!("{base}[context]\nbudget_tokens = 0\n"),
                "budget_tokens".to_string(),
            ),
            (
                format!("{base}[context.forget.keep_last_per_tool]\npage_snapshot = 0\n"),
                "keep_last_per_tool".to_string(),
            ),
        ] {
            let err = AgentDefinition::parse(&src, "hugr.toml").unwrap_err();
            assert!(matches!(err, ManifestError::Validate { .. }), "{src}");
            assert!(err.to_string().contains(&needle), "{err}");
        }
    }

    #[test]
    fn cron_jobs_parse_and_validate() {
        let src = r#"
[agent]
name = "x"

[models.medium]
model = "m"

[cron.daily]
schedule = "0 8 * * *"
question = "Write the daily summary."
lineage = "chain"

[cron.daily.limits]
max_cost_micro_usd = 100
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        assert_eq!(def.cron.len(), 1);
        let job = &def.cron[0];
        assert_eq!(job.name, "daily");
        assert_eq!(job.schedule, "0 8 * * *");
        assert_eq!(job.lineage, "chain");
        assert_eq!(job.limits.max_cost_micro_usd, Some(100));
    }

    #[test]
    fn invalid_cron_is_a_validation_error() {
        let base = "[agent]\nname = \"x\"\n[models.medium]\nmodel = \"m\"\n";
        for (src, needle) in [
            (
                format!("{base}[cron.bad]\nschedule = \"* * * * * *\"\nquestion = \"q\"\n"),
                "five-field".to_string(),
            ),
            (
                format!("{base}[cron.bad]\nschedule = \"99 * * * *\"\nquestion = \"q\"\n"),
                "schedule".to_string(),
            ),
            (
                format!("{base}[cron.bad]\nschedule = \"* * * * *\"\nquestion = \"\"\n"),
                "question".to_string(),
            ),
            (
                format!(
                    "{base}[cron.bad]\nschedule = \"* * * * *\"\nquestion = \"q\"\nlineage = \"forkish\"\n"
                ),
                "lineage".to_string(),
            ),
        ] {
            let err = AgentDefinition::parse(&src, "hugr.toml").unwrap_err();
            assert!(matches!(err, ManifestError::Validate { .. }), "{src}");
            assert!(err.to_string().contains(&needle), "{err}");
        }
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

[tools.web_fetch]
allow_hosts = ["api.example.com"]

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

    #[test]
    fn runtime_args_parse() {
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
env = "DOCS_PATH"
help = "Docs folder."
"#;
        let def = AgentDefinition::parse(src, "hugr.toml").unwrap();
        assert_eq!(def.runtime.args.len(), 1);
        let arg = &def.runtime.args[0];
        assert_eq!(arg.name, "docs_path");
        assert_eq!(arg.target, "tools.fs_read.root");
        assert!(arg.positional);
        assert!(arg.required);
        assert_eq!(arg.env.as_deref(), Some("DOCS_PATH"));
    }

    #[test]
    fn response_rust_type_is_not_a_manifest_key() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[response]
rust_type = "hugr_docs::DocsResponse"
"#;
        let err = AgentDefinition::parse(src, "hugr.toml").unwrap_err();
        assert!(err.to_string().contains("unknown field `rust_type`"));
    }

    #[test]
    fn response_schema_file_loads_relative_to_definition() {
        let dir = tempdir();
        write_test_cargo_toml(dir.path());
        std::fs::write(
            dir.path().join("hugr.toml"),
            r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[response]
schema = "schemas/response.json"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("schemas")).unwrap();
        std::fs::write(
            dir.path().join("schemas/response.json"),
            r#"{"type":"object","required":["response"]}"#,
        )
        .unwrap();

        let def = AgentDefinition::load(dir.path()).unwrap();
        assert_eq!(
            def.response_schema.as_ref().unwrap()["required"],
            serde_json::json!(["response"])
        );
    }

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir() -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("hugr-toolkit-manifest-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    fn write_test_cargo_toml(dir: &Path) {
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"test-agent\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
    }
}
