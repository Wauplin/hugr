//! Runtime argument application for definitions.
//!
//! A runtime argument is declared in `huggr.toml` and patched into a cloned
//! [`AgentDefinition`] before assembly. This keeps per-agent invocation config
//! in the manifest while letting the toolkit generate the CLI and MCP argument
//! surface from one place.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::manifest::{AgentDefinition, ToolKind};

/// String values supplied for declared runtime arguments, keyed by arg name.
pub type RuntimeValues = BTreeMap<String, String>;

/// Failure to resolve or apply runtime arguments.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeArgError {
    /// A required argument had no explicit value, env fallback, or default.
    #[error("missing required runtime argument `{0}`")]
    Missing(String),
    /// The manifest target cannot be patched.
    #[error("runtime argument `{arg}` targets `{target}`: {message}")]
    Target {
        arg: String,
        target: String,
        message: String,
    },
}

/// Resolve env/default fallbacks and apply all declared runtime values to `def`.
pub fn apply_runtime_values(
    def: &mut AgentDefinition,
    explicit: &RuntimeValues,
) -> Result<(), RuntimeArgError> {
    let args = def.runtime.args.clone();
    for arg in &args {
        let value = explicit
            .get(&arg.name)
            .cloned()
            .or_else(|| {
                arg.env
                    .as_deref()
                    .and_then(|name| std::env::var(name).ok())
                    .filter(|value| !value.is_empty())
            })
            .or_else(|| arg.default.clone());
        match value {
            Some(value) => {
                let value = normalize_runtime_value(&arg.target, value);
                apply_target(def, &arg.name, &arg.target, value)?
            }
            None if arg.required => return Err(RuntimeArgError::Missing(arg.name.clone())),
            None => {}
        }
    }
    Ok(())
}

fn normalize_runtime_value(target: &str, value: String) -> String {
    if !is_path_like_target(target) {
        return value;
    }
    let path = std::path::Path::new(&value);
    if path.is_absolute() {
        return value;
    }
    std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(path)
        .to_string_lossy()
        .into_owned()
}

fn is_path_like_target(target: &str) -> bool {
    let parts: Vec<&str> = target.split('.').collect();
    matches!(
        parts.as_slice(),
        ["tools", _, "root" | "file" | "artifact"]
            | ["tools", "mcp" | "agent", _, "root" | "file" | "artifact"]
            | ["traces", "store"]
            | ["scratchpad", "root"]
    )
}

fn apply_target(
    def: &mut AgentDefinition,
    arg: &str,
    target: &str,
    value: String,
) -> Result<(), RuntimeArgError> {
    let parts: Vec<&str> = target.split('.').collect();
    let fail = |message: String| RuntimeArgError::Target {
        arg: arg.to_string(),
        target: target.to_string(),
        message,
    };
    match parts.as_slice() {
        ["tools", name, key] => {
            let Some(grant) = def.tools.iter_mut().find(|grant| {
                grant.kind == ToolKind::Library && grant.name.as_str() == *name
            }) else {
                return Err(fail(format!("no library tool grant named `{name}`")));
            };
            set_object_string(&mut grant.config, key, value);
            Ok(())
        }
        ["tools", namespace, name, key] => {
            let kind = match *namespace {
                "mcp" => ToolKind::Mcp,
                "agent" => ToolKind::Agent,
                other => {
                    return Err(fail(format!(
                        "unknown tools namespace `{other}` (expected `mcp` or `agent`)"
                    )));
                }
            };
            let Some(grant) = def
                .tools
                .iter_mut()
                .find(|grant| grant.kind == kind && grant.name.as_str() == *name)
            else {
                return Err(fail(format!(
                    "no {namespace} tool grant named `{name}`"
                )));
            };
            set_object_string(&mut grant.config, key, value);
            Ok(())
        }
        ["models", "base_url"] => {
            def.models.base_url = Some(value);
            Ok(())
        }
        ["models", "api_key_env"] => {
            def.models.api_key_env = Some(value);
            Ok(())
        }
        ["models", tier, key] => {
            let Some(tier) = def.models.tiers.get_mut(*tier) else {
                return Err(fail(format!("no model tier named `{tier}`")));
            };
            match *key {
                "model" => tier.model = value,
                "input_usd_per_m_tokens" => {
                    tier.input_usd_per_m_tokens = Some(value.parse().map_err(|_| {
                        fail("input_usd_per_m_tokens must be a number".to_string())
                    })?)
                }
                "output_usd_per_m_tokens" => {
                    tier.output_usd_per_m_tokens = Some(value.parse().map_err(|_| {
                        fail("output_usd_per_m_tokens must be a number".to_string())
                    })?)
                }
                other => return Err(fail(format!("unknown model tier key `{other}`"))),
            }
            Ok(())
        }
        ["traces", "store"] => {
            def.traces.store = Some(value);
            Ok(())
        }
        ["scratchpad", "root"] => {
            def.scratchpad.root = Some(value);
            Ok(())
        }
        _ => Err(fail(
            "supported targets are tools.<grant>.<key>, tools.agent.<grant>.<key>, tools.mcp.<grant>.<key>, models.*, traces.store, and scratchpad.root"
                .to_string(),
        )),
    }
}

fn set_object_string(value: &mut Value, key: &str, string: String) {
    if !value.is_object() {
        *value = serde_json::json!({});
    }
    value
        .as_object_mut()
        .expect("object after initialization")
        .insert(key.to_string(), Value::String(string));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::AgentDefinition;

    #[test]
    fn applies_runtime_value_to_tool_scope() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[tools.fs_read]
root = "."
[runtime.args.docs_path]
target = "tools.fs_read.root"
required = true
"#;
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        let values = RuntimeValues::from([("docs_path".to_string(), "/tmp/docs".to_string())]);
        apply_runtime_values(&mut def, &values).unwrap();
        assert_eq!(def.tools[0].config["root"], "/tmp/docs");
    }

    #[test]
    fn relative_runtime_paths_are_cwd_relative() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[tools.fs_read]
root = "."
[runtime.args.docs_path]
target = "tools.fs_read.root"
"#;
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        let values = RuntimeValues::from([("docs_path".to_string(), "docs".to_string())]);
        apply_runtime_values(&mut def, &values).unwrap();
        assert_eq!(
            def.tools[0].config["root"],
            std::env::current_dir()
                .unwrap()
                .join("docs")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn required_runtime_arg_is_enforced() {
        let src = r#"
[agent]
name = "docs"
[models.medium]
model = "m"
[runtime.args.docs_path]
target = "tools.fs_read.root"
required = true
"#;
        let mut def = AgentDefinition::parse(src, "huggr.toml").unwrap();
        let err = apply_runtime_values(&mut def, &RuntimeValues::new()).unwrap_err();
        assert!(matches!(err, RuntimeArgError::Missing(_)), "{err}");
    }
}
