//! `hugr new`: scaffold a working definition folder (ROADMAP T1.4).
//!
//! Emits a folder with a commented `hugr.toml`, a `SYSTEM.md` prompt (using the
//! template vars `hugr run` substitutes), and any scaffolding a template needs
//! to be runnable immediately (e.g. the `docs` template creates the `docs/`
//! folder its `fs_read` root points at). The goal (exit criterion): `hugr new`
//! → edit one path → `hugr run` answers within minutes.

use std::path::{Path, PathBuf};

/// A starting template selectable with `hugr new --template`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Template {
    /// A docs-Q&A agent: `fs_read` jailed to a `docs/` folder.
    Docs,
    /// A database-Q&A agent: read-only `sqlite_query` on one file.
    Sqlite,
    /// No tools but the scratchpad — a blank starting point.
    Blank,
}

impl Template {
    /// Parse the `--template` value.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "docs" => Some(Self::Docs),
            "sqlite" => Some(Self::Sqlite),
            "blank" => Some(Self::Blank),
            _ => None,
        }
    }

    /// The template name (for diagnostics).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Docs => "docs",
            Self::Sqlite => "sqlite",
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
#[non_exhaustive]
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
    let mut files = vec![
        ScaffoldFile {
            rel_path: PathBuf::from("hugr.toml"),
            contents: manifest_for(name, template),
        },
        ScaffoldFile {
            rel_path: PathBuf::from("SYSTEM.md"),
            contents: system_for(name, template),
        },
    ];
    if template == Template::Docs {
        files.push(ScaffoldFile {
            rel_path: PathBuf::from("docs/README.md"),
            contents: format!(
                "# {name} docs\n\nPut the documents this agent should answer from in this folder.\n"
            ),
        });
    }
    files
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

fn tool_block(template: Template) -> &'static str {
    match template {
        Template::Docs => {
            "# Read-only, jailed to the docs/ folder beside this manifest.\n\
             [tools.fs_read]\n\
             root = \"./docs\"\n"
        }
        Template::Sqlite => {
            "# Read-only, scoped to a single SQLite file. Point `file` at your db.\n\
             # (Build the toolkit with `--features sqlite` to enable this tool.)\n\
             [tools.sqlite_query]\n\
             file = \"./data.db\"\n"
        }
        Template::Blank => {
            "# No external tools — this agent has only its scratchpad. Add a\n\
             # library grant here, e.g. [tools.fs_read] root = \"./data\".\n"
        }
    }
}

fn manifest_for(name: &str, template: Template) -> String {
    format!(
        "# Hugr agent definition — edit, then run with:\n\
         #   {name} <question>            (a built binary)\n\
         #   hugr run . \"<question>\"      (from this folder)\n\
         # Set the provider key first:   export HUGR_API_KEY=...\n\
         \n\
         [agent]\n\
         name = \"{name}\"\n\
         version = \"0.1.0\"\n\
         description = \"TODO: one line describing what this agent answers.\"\n\
         \n\
         [models]\n\
         base_url = \"https://router.huggingface.co/v1\"\n\
         api_key_env = \"HUGR_API_KEY\"\n\
         default = \"medium\"\n\
         \n\
         [models.medium]\n\
         model = \"google/gemma-4-31B-it:cerebras\"\n\
         input_usd_per_m_tokens = 1.0\n\
         output_usd_per_m_tokens = 1.5\n\
         # temperature = 0.2\n\
         \n\
         {tools}\n\
         [limits]\n\
         max_model_calls = 20\n\
         max_cost_micro_usd = 50000\n\
         timeout_s = 120\n",
        name = name,
        tools = tool_block(template),
    )
}

fn system_for(name: &str, template: Template) -> String {
    let role = match template {
        Template::Docs => {
            "You answer questions using only the documents available through your read-only file tools. \
             Search and read the sources you need before answering; if the docs lack the evidence, say so \
             rather than guessing."
        }
        Template::Sqlite => {
            "You answer questions about the data in your read-only SQLite database. Write SELECT queries \
             with `sqlite_query` to gather the facts you need, then answer from the results."
        }
        Template::Blank => {
            "You are a focused subagent. Answer the user's question. TODO: describe your task and how to \
             use your tools."
        }
    };
    format!(
        "# {name}\n\n\
         You are **{{{{agent_name}}}}**. {role}\n\n\
         Available tools: {{{{tools}}}}.\n\
         Today's date is {{{{date}}}}.\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentDefinition;

    #[test]
    fn parse_templates() {
        assert_eq!(Template::parse("docs"), Some(Template::Docs));
        assert_eq!(Template::parse("sqlite"), Some(Template::Sqlite));
        assert_eq!(Template::parse("blank"), Some(Template::Blank));
        assert_eq!(Template::parse("nope"), None);
    }

    #[test]
    fn scaffolded_manifest_parses_for_every_template() {
        for template in [Template::Docs, Template::Sqlite, Template::Blank] {
            let files = scaffold_files("my-agent", template);
            let manifest = &files[0];
            assert_eq!(manifest.rel_path, PathBuf::from("hugr.toml"));
            let def = AgentDefinition::parse(&manifest.contents, "hugr.toml").unwrap_or_else(|e| {
                panic!("template {} manifest must parse: {e}", template.as_str())
            });
            assert_eq!(def.agent.name, "my-agent");
            assert_eq!(def.default_tier(), Some("medium"));
            assert!(
                def.warnings.is_empty(),
                "template {} has warnings: {:?}",
                template.as_str(),
                def.warnings
            );
            // SYSTEM.md carries the template vars for hugr run to substitute.
            assert!(files[1].contents.contains("{{agent_name}}"));
        }
    }

    #[test]
    fn docs_template_creates_its_root_folder() {
        let files = scaffold_files("d", Template::Docs);
        assert!(
            files
                .iter()
                .any(|f| f.rel_path == PathBuf::from("docs/README.md"))
        );
    }
}
