//! Map the Python `Agent(...)` config (JSON, manifest-shaped keys) onto an
//! in-memory [`AgentDefinition`], so assembly goes through the exact same
//! `huggr_toolkit::runtime::build_agent` path as `huggr run` — parity by
//! construction, not by a parallel wiring.

use std::collections::BTreeMap;

use huggr_toolkit::manifest::{
    AgentDefinition, AgentMeta, ModelsConfig, ScratchpadConfig, TierConfig, ToolGrant, ToolKind,
    TracesConfig,
};
use huggr_toolkit::models::{
    default_catalog, load_global_catalog_if_exists, resolve_runtime_definition,
    resolve_source_definition, ModelCatalog,
};
use serde_json::Value;

/// Keys of the `models` dict that are provider knobs, not tier tables — the
/// same reserved set as the `[models]` manifest block.
const MODEL_KNOBS: &[&str] = &["default"];

pub fn definition_from_config(cfg: &Value) -> Result<AgentDefinition, String> {
    let obj = cfg.as_object().ok_or("agent config must be an object")?;
    let name = obj
        .get("name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
        .ok_or("agent config requires a non-empty `name`")?;

    let def = AgentDefinition {
        agent: AgentMeta {
            name: name.to_string(),
            version: str_field(obj.get("version")).unwrap_or_default(),
            description: str_field(obj.get("description")).unwrap_or_default(),
        },
        models: models_from(obj.get("models"))?,
        providers: section(obj.get("providers"), "providers")?,
        model_sources: Default::default(),
        tools: grants_from(obj.get("grants"))?,
        skills: Vec::new(),
        limits: section(obj.get("limits"), "limits")?,
        context: section(obj.get("context"), "context")?,
        scratchpad: ScratchpadConfig::default(),
        traces: TracesConfig::default(),
        runtime: Default::default(),
        response: Default::default(),
        response_schema: obj.get("response_schema").cloned().filter(|v| !v.is_null()),
        system_prompt: str_field(obj.get("system")),
        source_dir: None,
    };
    let mut def = def;
    if let Some(store) = obj.get("traces").and_then(Value::as_str) {
        def.traces.store = Some(store.to_string());
    }
    if let Some(root) = obj.get("scratchpad").and_then(Value::as_str) {
        def.scratchpad.root = Some(root.to_string());
    }
    // Run the same [context] semantic checks parse() applies to a TOML
    // manifest (compaction mode, forget values, and the summary tier against
    // the fixed set), so an invalid config is rejected here rather than at
    // runtime. This subsumes the standalone summary-tier check.
    def.validate_semantics("python agent config")
        .map_err(|err| err.to_string())?;
    let explicit = obj
        .get("model_overrides")
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value::<ModelCatalog>(value.clone()))
        .transpose()
        .map_err(|error| format!("invalid `model_overrides`: {error}"))?;
    let global = load_global_catalog_if_exists().map_err(|error| error.to_string())?;
    if let Some(explicit) = explicit.as_ref() {
        resolve_runtime_definition(&def, Some(explicit), global.as_ref())
            .map_err(|error| error.to_string())
    } else {
        resolve_source_definition(&def, global.as_ref().unwrap_or(&default_catalog()))
            .map_err(|error| error.to_string())
    }
}

fn str_field(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

fn section<T: Default + serde::de::DeserializeOwned>(
    value: Option<&Value>,
    what: &str,
) -> Result<T, String> {
    match value {
        None | Some(Value::Null) => Ok(T::default()),
        Some(value) => {
            serde_json::from_value(value.clone()).map_err(|err| format!("invalid `{what}`: {err}"))
        }
    }
}

/// `models` mirrors the `[models]` manifest block: reserved knob keys plus one
/// nested table per tier.
fn models_from(value: Option<&Value>) -> Result<ModelsConfig, String> {
    let Some(value) = value else {
        return Ok(ModelsConfig::default());
    };
    let obj = value.as_object().ok_or("`models` must be an object")?;
    let mut models = ModelsConfig {
        default: str_field(obj.get("default")).or_else(|| Some("balanced".to_string())),
        tiers: BTreeMap::new(),
    };
    for (key, tier) in obj {
        if MODEL_KNOBS.contains(&key.as_str()) {
            continue;
        }
        let tier: TierConfig = serde_json::from_value(tier.clone())
            .map_err(|err| format!("invalid model tier `{key}`: {err}"))?;
        if tier.model.trim().is_empty() {
            return Err(format!("model tier `{key}` requires a `model` id"));
        }
        models.tiers.insert(key.clone(), tier);
    }
    Ok(models)
}

/// `grants` mirrors the `[tools]` manifest block: library grants keyed by tool
/// id, plus the `mcp` / `agent` namespaces for external grants.
fn grants_from(value: Option<&Value>) -> Result<Vec<ToolGrant>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let obj = value.as_object().ok_or("`grants` must be an object")?;
    let mut grants = Vec::new();
    for (key, config) in obj {
        let kind = match key.as_str() {
            "mcp" => ToolKind::Mcp,
            "agent" => ToolKind::Agent,
            _ => {
                grants.push(ToolGrant {
                    name: key.clone(),
                    kind: ToolKind::Library,
                    config: config.clone(),
                });
                continue;
            }
        };
        let instances = config
            .as_object()
            .ok_or_else(|| format!("`grants.{key}` must be an object of instances"))?;
        for (name, instance) in instances {
            grants.push(ToolGrant {
                name: name.clone(),
                kind,
                config: instance.clone(),
            });
        }
    }
    grants.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(grants)
}
