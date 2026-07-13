//! Fixed-tier model configuration and host-side resolution.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::manifest::{AgentDefinition, MODEL_TIERS, ModelResolution, ProviderConfig, TierConfig};

pub const MODELS_FILE_ENV: &str = "HUGGR_MODELS_FILE";
pub const MODEL_SNAPSHOT_FILE: &str = ".huggr-models.snapshot.toml";

pub const DEFAULT_MODELS_TOML: &str = r#"[providers.hf]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HF_TOKEN"

[models.fast]
provider = "hf"
model = "deepseek-ai/DeepSeek-V4-Flash:fireworks-ai"
input_usd_per_m_tokens = 0.14
output_usd_per_m_tokens = 0.28

[models.balanced]
provider = "hf"
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[models.powerful]
provider = "hf"
model = "zai-org/GLM-5.2:together"
input_usd_per_m_tokens = 1.4
output_usd_per_m_tokens = 4.4
"#;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCatalog {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
    #[serde(default)]
    pub models: BTreeMap<String, TierConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum ModelConfigError {
    #[error("reading model configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("writing default model configuration {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid model configuration {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("model configuration defines no fixed tier")]
    Empty,
    #[error("model tier `{tier}` references unknown provider `{provider}`")]
    UnknownProvider { tier: String, provider: String },
    #[error("model tier `{tier}` has an empty model id")]
    EmptyModel { tier: String },
    #[error("unknown model tier `{0}`; expected fast, balanced, powerful, or max")]
    UnknownTier(String),
    #[error("built bundle has no resolved model snapshot at {0}")]
    MissingSnapshot(PathBuf),
}

impl ModelCatalog {
    pub fn parse(src: &str, path: impl AsRef<Path>) -> Result<Self, ModelConfigError> {
        let path = path.as_ref();
        let catalog: Self = toml::from_str(src).map_err(|err| ModelConfigError::Parse {
            path: path.to_path_buf(),
            message: err.to_string(),
        })?;
        catalog.validate()?;
        Ok(catalog)
    }

    pub fn to_toml(&self) -> Result<String, ModelConfigError> {
        toml::to_string_pretty(self).map_err(|err| ModelConfigError::Parse {
            path: PathBuf::from(MODEL_SNAPSHOT_FILE),
            message: err.to_string(),
        })
    }

    pub fn validate(&self) -> Result<(), ModelConfigError> {
        if self.models.is_empty() {
            return Err(ModelConfigError::Empty);
        }
        for (tier, model) in &self.models {
            if !MODEL_TIERS.contains(&tier.as_str()) {
                return Err(ModelConfigError::UnknownTier(tier.clone()));
            }
            if model.model.trim().is_empty() {
                return Err(ModelConfigError::EmptyModel { tier: tier.clone() });
            }
            if !self.providers.contains_key(&model.provider) {
                return Err(ModelConfigError::UnknownProvider {
                    tier: tier.clone(),
                    provider: model.provider.clone(),
                });
            }
        }
        Ok(())
    }
}

pub fn default_catalog() -> ModelCatalog {
    ModelCatalog::parse(DEFAULT_MODELS_TOML, "<built-in models>")
        .expect("built-in model catalog is valid")
}

pub fn models_file_path() -> PathBuf {
    models_file_path_from(|key| std::env::var_os(key), std::env::temp_dir())
}

fn models_file_path_from(env: impl Fn(&str) -> Option<OsString>, temp_dir: PathBuf) -> PathBuf {
    if let Some(path) = env(MODELS_FILE_ENV)
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    if let Some(base) = env("HUGGR_HOME")
        && !base.is_empty()
    {
        return PathBuf::from(base).join("models.toml");
    }
    if let Some(home) = env("HOME")
        && !home.is_empty()
    {
        return PathBuf::from(home).join(".huggr").join("models.toml");
    }
    temp_dir.join(".huggr").join("models.toml")
}

pub fn load_catalog(path: impl AsRef<Path>) -> Result<ModelCatalog, ModelConfigError> {
    let path = path.as_ref();
    let src = std::fs::read_to_string(path).map_err(|source| ModelConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    ModelCatalog::parse(&src, path)
}

pub fn load_global_catalog_if_exists() -> Result<Option<ModelCatalog>, ModelConfigError> {
    let path = models_file_path();
    match load_catalog(&path) {
        Ok(catalog) => Ok(Some(catalog)),
        Err(ModelConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

pub fn load_or_create_global_catalog() -> Result<ModelCatalog, ModelConfigError> {
    load_or_create_catalog_at(models_file_path())
}

fn load_or_create_catalog_at(path: PathBuf) -> Result<ModelCatalog, ModelConfigError> {
    match load_catalog(&path) {
        Ok(catalog) => return Ok(catalog),
        Err(ModelConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ModelConfigError::Write {
            path: path.clone(),
            source,
        })?;
    }
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => file
            .write_all(DEFAULT_MODELS_TOML.as_bytes())
            .map_err(|source| ModelConfigError::Write {
                path: path.clone(),
                source,
            })?,
        Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(ModelConfigError::Write {
                path: path.clone(),
                source,
            });
        }
    }
    load_catalog(path)
}

pub fn resolve_source_definition(
    def: &AgentDefinition,
    catalog: &ModelCatalog,
) -> Result<AgentDefinition, ModelConfigError> {
    catalog.validate()?;
    resolve_source_definition_with_env(def, catalog, |name| std::env::var(name).ok())
}

fn resolve_source_definition_with_env(
    def: &AgentDefinition,
    catalog: &ModelCatalog,
    env: impl Fn(&str) -> Option<String>,
) -> Result<AgentDefinition, ModelConfigError> {
    catalog.validate()?;
    let mut resolved = def.clone();
    resolved.providers = catalog.providers.clone();
    resolved.providers.extend(def.providers.clone());
    resolved.models.tiers.clear();
    resolved.model_sources.clear();

    for tier in MODEL_TIERS {
        let (model, source, resolved_from) = if let Some(model) = def.models.tiers.get(tier) {
            (model.clone(), "manifest", tier.to_string())
        } else if let Some(model_id) = env(&model_env_name(tier)).filter(|v| !v.trim().is_empty()) {
            let (mut model, from) = closest_model(catalog, tier)?;
            model.model = model_id;
            (model, "environment", from)
        } else {
            let (model, from) = closest_model(catalog, tier)?;
            (model, "global", from)
        };
        validate_provider(&resolved.providers, tier, &model)?;
        resolved.models.tiers.insert(tier.to_string(), model);
        resolved.model_sources.insert(
            tier.to_string(),
            ModelResolution {
                source: source.to_string(),
                resolved_from,
            },
        );
    }
    Ok(resolved)
}

pub fn resolve_bundled_definition(
    def: &AgentDefinition,
    explicit: Option<&ModelCatalog>,
) -> Result<AgentDefinition, ModelConfigError> {
    let global = load_global_catalog_if_exists()?;
    resolve_bundled_definition_with_host(def, explicit, global.as_ref(), |name| {
        std::env::var(name).ok()
    })
}

fn resolve_bundled_definition_with_host(
    def: &AgentDefinition,
    explicit: Option<&ModelCatalog>,
    global: Option<&ModelCatalog>,
    env: impl Fn(&str) -> Option<String>,
) -> Result<AgentDefinition, ModelConfigError> {
    let source_dir = def.source_dir.as_deref().unwrap_or_else(|| Path::new("."));
    let snapshot_path = source_dir.join(MODEL_SNAPSHOT_FILE);
    let snapshot = match load_catalog(&snapshot_path) {
        Ok(catalog) => catalog,
        Err(ModelConfigError::Read { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            return Err(ModelConfigError::MissingSnapshot(snapshot_path));
        }
        Err(error) => return Err(error),
    };
    let (catalog, label) = if let Some(catalog) = explicit {
        (catalog, "runtime")
    } else if let Some(catalog) = global {
        (catalog, "global")
    } else {
        (&snapshot, "bundled")
    };
    resolve_catalog_definition_with_env(def, catalog, label, env)
}

pub fn resolve_runtime_definition(
    def: &AgentDefinition,
    explicit: Option<&ModelCatalog>,
    global: Option<&ModelCatalog>,
) -> Result<AgentDefinition, ModelConfigError> {
    let fallback = default_catalog();
    let (catalog, label) = if let Some(catalog) = explicit {
        (catalog, "runtime")
    } else if let Some(catalog) = global {
        (catalog, "global")
    } else {
        (&fallback, "built_in")
    };
    resolve_catalog_definition_with_env(def, catalog, label, |name| std::env::var(name).ok())
}

fn resolve_catalog_definition_with_env(
    def: &AgentDefinition,
    catalog: &ModelCatalog,
    label: &str,
    env: impl Fn(&str) -> Option<String>,
) -> Result<AgentDefinition, ModelConfigError> {
    catalog.validate()?;
    let mut resolved = def.clone();
    resolved.providers = catalog.providers.clone();
    resolved.models.tiers.clear();
    resolved.model_sources.clear();
    for tier in MODEL_TIERS {
        let (mut model, from) = closest_model(catalog, tier)?;
        let source =
            if let Some(model_id) = env(&model_env_name(tier)).filter(|v| !v.trim().is_empty()) {
                model.model = model_id;
                "environment"
            } else {
                label
            };
        validate_provider(&resolved.providers, tier, &model)?;
        resolved.models.tiers.insert(tier.to_string(), model);
        resolved.model_sources.insert(
            tier.to_string(),
            ModelResolution {
                source: source.to_string(),
                resolved_from: from,
            },
        );
    }
    Ok(resolved)
}

pub fn catalog_from_resolved(def: &AgentDefinition) -> ModelCatalog {
    let provider_names: std::collections::BTreeSet<_> = def
        .models
        .tiers
        .values()
        .map(|model| model.provider.as_str())
        .collect();
    ModelCatalog {
        providers: def
            .providers
            .iter()
            .filter(|(name, _)| provider_names.contains(name.as_str()))
            .map(|(name, provider)| (name.clone(), provider.clone()))
            .collect(),
        models: def.models.tiers.clone(),
    }
}

fn closest_model(
    catalog: &ModelCatalog,
    requested: &str,
) -> Result<(TierConfig, String), ModelConfigError> {
    let requested_index = MODEL_TIERS
        .iter()
        .position(|tier| *tier == requested)
        .ok_or_else(|| ModelConfigError::UnknownTier(requested.to_string()))?;
    for distance in 0..MODEL_TIERS.len() {
        if let Some(index) = requested_index.checked_sub(distance) {
            let tier = MODEL_TIERS[index];
            if let Some(model) = catalog.models.get(tier) {
                return Ok((model.clone(), tier.to_string()));
            }
        }
        let index = requested_index + distance;
        if distance > 0 && index < MODEL_TIERS.len() {
            let tier = MODEL_TIERS[index];
            if let Some(model) = catalog.models.get(tier) {
                return Ok((model.clone(), tier.to_string()));
            }
        }
    }
    Err(ModelConfigError::Empty)
}

fn validate_provider(
    providers: &BTreeMap<String, ProviderConfig>,
    tier: &str,
    model: &TierConfig,
) -> Result<(), ModelConfigError> {
    if providers.contains_key(&model.provider) {
        Ok(())
    } else {
        Err(ModelConfigError::UnknownProvider {
            tier: tier.to_string(),
            provider: model.provider.clone(),
        })
    }
}

fn model_env_name(tier: &str) -> String {
    format!("HUGGR_MODEL_{}", tier.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AgentDefinition;

    fn definition(src: &str) -> AgentDefinition {
        AgentDefinition::parse(src, "huggr.toml").unwrap()
    }

    #[test]
    fn missing_tiers_prefer_the_nearest_lower_tier() {
        let catalog = ModelCatalog::parse(
            r#"[providers.p]
base_url = "https://example.com/v1"
api_key_env = "KEY"
[models.balanced]
provider = "p"
model = "b"
[models.powerful]
provider = "p"
model = "p"
"#,
            "models.toml",
        )
        .unwrap();
        let def = definition("[agent]\nname='x'\n[models]\ndefault='max'\n");
        let resolved = resolve_source_definition_with_env(&def, &catalog, |_| None).unwrap();
        assert_eq!(resolved.models.tiers["fast"].model, "b");
        assert_eq!(resolved.models.tiers["max"].model, "p");
        assert_eq!(resolved.model_sources["max"].resolved_from, "powerful");
    }

    #[test]
    fn manifest_then_environment_then_catalog_precedence() {
        let catalog = default_catalog();
        let def = definition(
            r#"[agent]
name = "x"
[models]
default = "powerful"
[providers.local]
base_url = "http://localhost:1234/v1"
api_key_env = "LOCAL_KEY"
[models.fast]
provider = "local"
model = "local-fast"
"#,
        );
        let resolved = resolve_source_definition_with_env(&def, &catalog, |name| {
            (name == "HUGGR_MODEL_BALANCED").then(|| "env-balanced".to_string())
        })
        .unwrap();
        assert_eq!(resolved.models.tiers["fast"].model, "local-fast");
        assert_eq!(resolved.model_sources["fast"].source, "manifest");
        assert_eq!(resolved.models.tiers["balanced"].model, "env-balanced");
        assert_eq!(resolved.model_sources["balanced"].source, "environment");
        assert_eq!(resolved.model_sources["powerful"].source, "global");
    }

    #[test]
    fn default_catalog_uses_requested_models_and_powerful_for_max() {
        let catalog = default_catalog();
        assert_eq!(catalog.models.len(), 3);
        assert!(!catalog.models.contains_key("max"));
        assert_eq!(
            catalog.models["fast"].model,
            "deepseek-ai/DeepSeek-V4-Flash:fireworks-ai"
        );
        assert_eq!(catalog.models["fast"].input_usd_per_m_tokens, Some(0.14));
        assert_eq!(catalog.models["fast"].output_usd_per_m_tokens, Some(0.28));
        assert_eq!(
            catalog.models["balanced"].model,
            "google/gemma-4-31B-it:cerebras"
        );
        assert_eq!(catalog.models["balanced"].input_usd_per_m_tokens, Some(1.0));
        assert_eq!(
            catalog.models["balanced"].output_usd_per_m_tokens,
            Some(1.5)
        );
        assert_eq!(catalog.models["powerful"].input_usd_per_m_tokens, Some(1.4));
        assert_eq!(
            catalog.models["powerful"].output_usd_per_m_tokens,
            Some(4.4)
        );

        let def = definition("[agent]\nname='x'\n[models]\ndefault='max'\n");
        let resolved = resolve_source_definition_with_env(&def, &catalog, |_| None).unwrap();
        assert_eq!(
            resolved.models.tiers["max"].model,
            "zai-org/GLM-5.2:together"
        );
        assert_eq!(resolved.model_sources["max"].resolved_from, "powerful");
    }

    #[test]
    fn first_load_writes_the_default_catalog() {
        let root = std::env::temp_dir().join(format!(
            "huggr-model-catalog-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        let path = root.join("nested").join("models.toml");
        let _ = std::fs::remove_dir_all(&root);

        let catalog = load_or_create_catalog_at(path.clone()).unwrap();
        assert_eq!(catalog, default_catalog());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_MODELS_TOML);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_host_catalog_replaces_the_bundled_snapshot() {
        let root =
            std::env::temp_dir().join(format!("huggr-bundled-model-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(MODEL_SNAPSHOT_FILE), DEFAULT_MODELS_TOML).unwrap();
        let mut def = definition("[agent]\nname='x'\n[models]\ndefault='max'\n");
        def.source_dir = Some(root.clone());
        let host = ModelCatalog::parse(
            r#"[providers.host]
base_url = "https://host.example/v1"
api_key_env = "HOST_KEY"
[models.balanced]
provider = "host"
model = "host-model"
"#,
            "host-models.toml",
        )
        .unwrap();

        let resolved =
            resolve_bundled_definition_with_host(&def, None, Some(&host), |_| None).unwrap();
        assert_eq!(resolved.models.tiers["max"].model, "host-model");
        assert_eq!(resolved.model_sources["max"].source, "global");
        assert_eq!(resolved.model_sources["max"].resolved_from, "balanced");

        std::fs::remove_dir_all(root).unwrap();
    }
}
