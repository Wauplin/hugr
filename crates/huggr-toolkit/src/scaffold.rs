//! `huggr new`: scaffold a working agent crate folder.
//!
//! Emits a folder with a minimal Rust crate, a commented `huggr.toml`, and a
//! `SYSTEM.md` prompt (using the template vars `huggr run` substitutes). The
//! goal: `huggr new` → edit one path → `huggr run` answers within minutes.
//!
//! The default `weather` template is the self-contained beginner example: it
//! grants only the allowlisted `web_fetch` tool (scoped to the Open-Meteo API
//! hosts in `huggr.toml`), so it needs no local data folder — `huggr new` → set
//! the key → `huggr run` answers immediately. Its one source of truth is the
//! checked-in `examples/huglet-weather` crate: the files are embedded at compile
//! time and the agent name is substituted on scaffold.

use std::path::{Path, PathBuf};

/// A starting template selectable with `huggr new --template`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Template {
    /// The self-contained beginner example: a weather assistant with only the
    /// allowlisted `web_fetch` tool (no local data folder required).
    Weather,
    /// No tools but the scratchpad — a blank starting point.
    Blank,
}

impl Template {
    /// Parse the `--template` value.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "weather" => Some(Self::Weather),
            "blank" => Some(Self::Blank),
            _ => None,
        }
    }

    /// The template name (for diagnostics).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Weather => "weather",
            Self::Blank => "blank",
        }
    }
}

/// One file the scaffold writes.
#[derive(Clone, Debug, PartialEq)]
pub struct ScaffoldFile {
    /// Path relative to the new agent folder.
    pub rel_path: PathBuf,
    /// File contents.
    pub contents: String,
}

/// Failure to scaffold.
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    /// The target folder already exists (never overwrite).
    #[error("target folder already exists: {0}")]
    Exists(PathBuf),
    /// A write failed.
    #[error("writing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The files a template emits for an agent named `name` (pure — no IO). Callers
/// can preview these before [`write_scaffold`] commits them to disk.
pub fn scaffold_files(name: &str, template: Template) -> Vec<ScaffoldFile> {
    match template {
        Template::Weather => weather_files(name),
        Template::Blank => vec![
            ScaffoldFile {
                rel_path: PathBuf::from("Cargo.toml"),
                contents: cargo_toml_for(name),
            },
            ScaffoldFile {
                rel_path: PathBuf::from("src/lib.rs"),
                contents: lib_rs_for(name),
            },
            ScaffoldFile {
                rel_path: PathBuf::from("huggr.toml"),
                contents: blank_manifest_for(name),
            },
            ScaffoldFile {
                rel_path: PathBuf::from("SYSTEM.md"),
                contents: blank_system().to_string(),
            },
        ],
    }
}

/// The `weather` template: the checked-in `examples/huglet-weather` crate,
/// embedded at compile time, with the agent name substituted.
fn weather_files(name: &str) -> Vec<ScaffoldFile> {
    const CARGO_TOML: &str = include_str!("../../../examples/huglet-weather/Cargo.toml");
    const LIB_RS: &str = include_str!("../../../examples/huglet-weather/src/lib.rs");
    const MANIFEST: &str = include_str!("../../../examples/huglet-weather/huggr.toml");
    const SYSTEM: &str = include_str!("../../../examples/huglet-weather/SYSTEM.md");
    const README: &str = include_str!("../../../examples/huglet-weather/README.md");

    let package = sanitize_rust_name(name, '-');
    let crate_name = package.replace('-', "_");
    let substitute = |source: &str| {
        source
            .replace("huglet_weather", &crate_name)
            .replace("huglet-weather", &package)
    };
    vec![
        ScaffoldFile {
            rel_path: PathBuf::from("Cargo.toml"),
            contents: substitute(CARGO_TOML),
        },
        ScaffoldFile {
            rel_path: PathBuf::from("src/lib.rs"),
            contents: substitute(LIB_RS),
        },
        ScaffoldFile {
            rel_path: PathBuf::from("huggr.toml"),
            contents: substitute(MANIFEST),
        },
        ScaffoldFile {
            rel_path: PathBuf::from("SYSTEM.md"),
            contents: SYSTEM.to_string(),
        },
        ScaffoldFile {
            rel_path: PathBuf::from("README.md"),
            contents: substitute(README),
        },
    ]
}

/// Scaffold `name` under `parent`, returning the created agent folder. Refuses
/// to overwrite an existing folder.
pub fn write_scaffold(
    parent: &Path,
    name: &str,
    template: Template,
) -> Result<PathBuf, ScaffoldError> {
    let dir = parent.join(name);
    if dir.exists() {
        return Err(ScaffoldError::Exists(dir));
    }
    for file in scaffold_files(name, template) {
        let path = dir.join(&file.rel_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ScaffoldError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        std::fs::write(&path, file.contents).map_err(|source| ScaffoldError::Io {
            path: path.clone(),
            source,
        })?;
    }
    Ok(dir)
}

fn blank_manifest_for(name: &str) -> String {
    format!(
        "# Huggr agent definition — edit, then run with:\n\
         #   {name} <question>            (a built binary)\n\
         #   huggr run . \"<question>\"      (from this folder)\n\
         # The first `huggr` run creates ~/.huggr/models.toml. Set the key named there.\n\
         \n\
         [agent]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         description = \"TODO: one line describing what this agent answers.\"\n\
         \n\
         [models]\n\
         default = \"balanced\"\n\
         \n\
         # No external tools — this agent has only its scratchpad. Add a\n\
         # library grant here, e.g. [tools.fs_read] root = \"./data\".\n\
         \n\
         # Limits are opt-in (unset = unbounded). Cap an ask with e.g.:\n\
         # [limits]\n\
         # max_cost_micro_usd = 50000\n",
    )
}

fn cargo_toml_for(name: &str) -> String {
    let package = sanitize_rust_name(name, '-');
    format!(
        "[package]\n\
         name = \"{package}\"\n\
         version = \"0.1.0\"\n\
         edition = \"2024\"\n\
         \n\
         [dependencies]\n\
         serde = {{ version = \"1\", features = [\"derive\"] }}\n\
         schemars = \"1\"\n"
    )
}

fn lib_rs_for(name: &str) -> String {
    let crate_name = sanitize_rust_name(name, '-').replace('-', "_");
    format!(
        "//! Rust response contract and extension point for the `{name}` Huggr agent.\n\
         \n\
         use schemars::JsonSchema;\n\
         use serde::{{Deserialize, Serialize}};\n\
         \n\
         pub const RESPONSE_RUST_TYPE: &str = \"{crate_name}::Response\";\n\
         \n\
         #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]\n\
         #[serde(deny_unknown_fields)]\n\
         pub struct Response {{\n\
             pub response: String,\n\
         }}\n"
    )
}

fn sanitize_rust_name(name: &str, separator: char) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                separator
            }
        })
        .collect();
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        out.insert_str(0, "agent-");
    }
    out
}

fn blank_system() -> &'static str {
    // Uses the {{agent_name}} template var `huggr run` substitutes at assembly.
    "You are **{{agent_name}}**. You are a focused huglet. Answer the user's question. TODO: describe your task and how to \
         use your tools."
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentDefinition;

    #[test]
    fn parse_templates() {
        assert_eq!(Template::parse("weather"), Some(Template::Weather));
        assert_eq!(Template::parse("blank"), Some(Template::Blank));
        assert_eq!(Template::parse("nope"), None);
    }

    #[test]
    fn scaffolded_manifest_parses_for_every_template() {
        for template in [Template::Weather, Template::Blank] {
            let files = scaffold_files("my-agent", template);
            let manifest = files
                .iter()
                .find(|file| file.rel_path == Path::new("huggr.toml"))
                .unwrap();
            assert_eq!(manifest.rel_path, PathBuf::from("huggr.toml"));
            let def =
                AgentDefinition::parse(&manifest.contents, "huggr.toml").unwrap_or_else(|e| {
                    panic!("template {} manifest must parse: {e}", template.as_str())
                });
            assert_eq!(def.agent.name, "my-agent");
            assert_eq!(def.default_tier(), Some("balanced"));
            // SYSTEM.md carries the template vars for huggr run to substitute.
            assert!(
                files
                    .iter()
                    .any(|file| file.rel_path == Path::new("SYSTEM.md")
                        && file.contents.contains("{{agent_name}}"))
            );
        }
    }

    #[test]
    fn scaffold_creates_a_rust_crate() {
        let files = scaffold_files("my-agent", Template::Blank);
        assert!(
            files
                .iter()
                .any(|file| file.rel_path == Path::new("Cargo.toml"))
        );
        assert!(
            files
                .iter()
                .any(|file| file.rel_path == Path::new("src/lib.rs"))
        );
    }

    #[test]
    fn weather_template_substitutes_the_agent_name_everywhere() {
        let files = scaffold_files("sky", Template::Weather);
        for file in &files {
            assert!(
                !file.contents.contains("huglet-weather")
                    && !file.contents.contains("huglet_weather"),
                "leftover template token in {}",
                file.rel_path.display()
            );
        }
        let lib = files
            .iter()
            .find(|f| f.rel_path == Path::new("src/lib.rs"))
            .unwrap();
        assert!(
            lib.contents.contains("\"sky::Response\""),
            "{}",
            lib.contents
        );
        let manifest = files
            .iter()
            .find(|f| f.rel_path == Path::new("huggr.toml"))
            .unwrap();
        let def = AgentDefinition::parse(&manifest.contents, "huggr.toml").unwrap();
        assert_eq!(def.agent.name, "sky");
    }

    #[test]
    fn weather_template_is_self_contained_and_grants_web_fetch() {
        let files = scaffold_files("sky", Template::Weather);
        let manifest = files
            .iter()
            .find(|f| f.rel_path == Path::new("huggr.toml"))
            .unwrap();
        let def = AgentDefinition::parse(&manifest.contents, "huggr.toml").unwrap();
        // Grants only the allowlisted web_fetch tool, scoped to Open-Meteo.
        let web = def
            .tools
            .iter()
            .find(|t| t.name == "web_fetch")
            .expect("weather template grants web_fetch");
        let hosts = web.config.get("allow_hosts").and_then(|v| v.as_array());
        let hosts: Vec<&str> = hosts.unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert!(hosts.contains(&"api.open-meteo.com"));
        assert!(hosts.contains(&"geocoding-api.open-meteo.com"));
        // Ships a README with next steps, and needs no local data folder.
        assert!(files.iter().any(|f| f.rel_path == Path::new("README.md")));
        assert!(
            !files
                .iter()
                .any(|f| f.rel_path.starts_with("docs") || f.rel_path.starts_with("data"))
        );
    }
}
