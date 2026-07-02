//! Host-side skill bundle discovery (ROADMAP_2 C4).
//!
//! A skill is a directory containing `SKILL.md` plus optional supporting files.
//! Discovery is host IO and stays out of `hugr-core`; the brain will only see
//! skill metadata later when the host threads it into a pure policy.

use std::path::{Path, PathBuf};

use hugr_core::ToolSchema;
use thiserror::Error;

/// One discovered skill bundle on disk.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SkillBundle {
    pub id: String,
    pub title: String,
    pub summary: Option<String>,
    pub root: PathBuf,
    pub instructions: String,
    /// Optional tool schemas contributed by `tools/*.json`. The host may wrap
    /// these in capabilities in a later step; for C4 they are discoverable
    /// metadata and never touch the core.
    pub tool_schemas: Vec<ToolSchema>,
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum SkillError {
    #[error("skill IO error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid skill tool schema at {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
}

/// Discover skills from the product's well-known locations. Missing locations
/// are ignored; malformed bundles surface as errors.
pub fn discover() -> Result<Vec<SkillBundle>, SkillError> {
    discover_from(well_known_dirs())
}

/// Discover skills from explicit roots. Each root may be either a skill bundle
/// itself or a directory containing many skill bundle directories.
pub fn discover_from(
    roots: impl IntoIterator<Item = impl Into<PathBuf>>,
) -> Result<Vec<SkillBundle>, SkillError> {
    let mut bundles = Vec::new();
    for root in roots {
        let root = root.into();
        if !root.exists() {
            continue;
        }
        if root.join("SKILL.md").is_file() {
            bundles.push(load_bundle(&root)?);
            continue;
        }
        let entries = std::fs::read_dir(&root).map_err(|source| SkillError::Io {
            path: root.clone(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| SkillError::Io {
                path: root.clone(),
                source,
            })?;
            let path = entry.path();
            if path.join("SKILL.md").is_file() {
                bundles.push(load_bundle(&path)?);
            }
        }
    }
    bundles.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(bundles)
}

fn load_bundle(root: &Path) -> Result<SkillBundle, SkillError> {
    let skill_md = root.join("SKILL.md");
    let instructions = std::fs::read_to_string(&skill_md).map_err(|source| SkillError::Io {
        path: skill_md.clone(),
        source,
    })?;
    let (title, summary) = parse_markdown_metadata(&instructions, root);
    Ok(SkillBundle {
        id: root
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string(),
        title,
        summary,
        root: root.to_path_buf(),
        tool_schemas: load_tool_schemas(root)?,
        instructions,
    })
}

fn parse_markdown_metadata(instructions: &str, root: &Path) -> (String, Option<String>) {
    let title = instructions
        .lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            root.file_name()
                .and_then(|s| s.to_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "skill".to_string());
    let summary = instructions
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned);
    (title, summary)
}

fn load_tool_schemas(root: &Path) -> Result<Vec<ToolSchema>, SkillError> {
    let tools_dir = root.join("tools");
    if !tools_dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut schemas = Vec::new();
    let entries = std::fs::read_dir(&tools_dir).map_err(|source| SkillError::Io {
        path: tools_dir.clone(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SkillError::Io {
            path: tools_dir.clone(),
            source,
        })?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).map_err(|source| SkillError::Io {
            path: path.clone(),
            source,
        })?;
        let schema: ToolSchema =
            serde_json::from_str(&text).map_err(|source| SkillError::Json {
                path: path.clone(),
                source,
            })?;
        schemas.push(schema);
    }
    schemas.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(schemas)
}

fn well_known_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(paths) = std::env::var_os("HUGR_SKILLS_DIR") {
        dirs.extend(std::env::split_paths(&paths));
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join(".hugr/skills"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join(".config/hugr/skills"));
    }
    dirs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use serde_json::json;
    use std::io::Write;

    #[test]
    fn discovers_skill_bundle_metadata_and_tool_schemas() {
        let root = TempDir::new("skill-discover");
        let skill = root.path().join("rust-reviewer");
        std::fs::create_dir_all(skill.join("tools")).unwrap();
        write_file(
            &skill.join("SKILL.md"),
            "# Rust Reviewer\n\nReview Rust diffs for correctness.\n",
        );
        write_file(
            &skill.join("tools/check.json"),
            &serde_json::to_string(&ToolSchema::new(
                "cargo_check",
                "Run cargo check.",
                json!({ "type": "object" }),
            ))
            .unwrap(),
        );

        let bundles = discover_from([root.path().to_path_buf()]).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].id, "rust-reviewer");
        assert_eq!(bundles[0].title, "Rust Reviewer");
        assert_eq!(
            bundles[0].summary.as_deref(),
            Some("Review Rust diffs for correctness.")
        );
        assert!(bundles[0].instructions.contains("Review Rust diffs"));
        assert_eq!(bundles[0].tool_schemas[0].name, "cargo_check");
    }

    fn write_file(path: &Path, text: &str) {
        let mut file = std::fs::File::create(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}
