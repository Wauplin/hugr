//! Standard `SKILL.md` discovery and progressive disclosure.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde::Deserialize;
use serde_json::json;

const MAX_SKILL_FILE_BYTES: u64 = 1_000_000;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("skill path {path} is not readable: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid skill at {path}: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error("duplicate skill name `{0}`")]
    Duplicate(String),
}

#[derive(Clone, Debug)]
struct Skill {
    name: String,
    description: String,
    root: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

pub fn discover_skills(paths: &[PathBuf]) -> Result<SkillSet, SkillError> {
    let mut set = SkillSet::default();
    for path in paths {
        let path = path.canonicalize().map_err(|source| SkillError::Io {
            path: path.clone(),
            source,
        })?;
        let mut files = Vec::new();
        find_skill_files(&path, &mut files)?;
        if files.is_empty() {
            return Err(SkillError::Invalid {
                path,
                message: "no SKILL.md found".to_string(),
            });
        }
        for file in files {
            let skill = parse_skill(&file)?;
            if set
                .skills
                .insert(skill.name.clone(), skill.clone())
                .is_some()
            {
                return Err(SkillError::Duplicate(skill.name));
            }
        }
    }
    Ok(set)
}

fn find_skill_files(path: &Path, out: &mut Vec<PathBuf>) -> Result<(), SkillError> {
    if path.is_file() {
        if path.file_name().and_then(|v| v.to_str()) == Some("SKILL.md") {
            out.push(path.to_path_buf());
            return Ok(());
        }
        return Err(SkillError::Invalid {
            path: path.to_path_buf(),
            message: "path must be a skill folder, skills folder, or SKILL.md".to_string(),
        });
    }
    let direct = path.join("SKILL.md");
    if direct.is_file() {
        out.push(direct);
        return Ok(());
    }
    let entries = fs::read_dir(path).map_err(|source| SkillError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| SkillError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            && entry.path().join("SKILL.md").is_file()
        {
            out.push(entry.path().join("SKILL.md"));
        }
    }
    out.sort();
    Ok(())
}

fn parse_skill(path: &Path) -> Result<Skill, SkillError> {
    let content = fs::read_to_string(path).map_err(|source| SkillError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let content = content.replace("\r\n", "\n");
    let rest = content
        .strip_prefix("---\n")
        .ok_or_else(|| invalid(path, "SKILL.md must start with YAML frontmatter"))?;
    let (yaml, _) = rest
        .split_once("\n---\n")
        .ok_or_else(|| invalid(path, "SKILL.md frontmatter is not closed"))?;
    let front: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|err| invalid(path, &format!("invalid YAML frontmatter: {err}")))?;
    let folder = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|v| v.to_str())
        .unwrap_or("");
    if front.name != folder {
        return Err(invalid(
            path,
            "frontmatter name must match the skill folder name",
        ));
    }
    if front.name.is_empty()
        || front.name.len() > 64
        || !front
            .name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || front.name.starts_with('-')
        || front.name.ends_with('-')
        || front.name.contains("--")
    {
        return Err(invalid(
            path,
            "name must be 1-64 lowercase letters, digits, or single hyphens",
        ));
    }
    if front.description.is_empty() || front.description.len() > 1024 {
        return Err(invalid(path, "description must contain 1-1024 bytes"));
    }
    Ok(Skill {
        name: front.name,
        description: front.description,
        root: path.parent().unwrap().to_path_buf(),
    })
}

fn invalid(path: &Path, message: &str) -> SkillError {
    SkillError::Invalid {
        path: path.to_path_buf(),
        message: message.to_string(),
    }
}

impl SkillSet {
    pub(crate) fn capabilities(&self) -> Vec<Arc<dyn Capability>> {
        if self.skills.is_empty() {
            Vec::new()
        } else {
            vec![Arc::new(SkillRead(self.clone()))]
        }
    }
}

pub(crate) fn skills_prompt(base: &str, skills: &SkillSet) -> String {
    if skills.skills.is_empty() {
        return base.to_string();
    }
    let catalog = skills
        .skills
        .values()
        .map(|skill| format!("- `{}`: {}", skill.name, skill.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{base}\n\n## Skills\n\nThe following skills are available. When a skill matches the user's task, call `skill_read` before acting and follow the returned instructions. Read referenced files with the same tool only when needed.\n\n{catalog}"
    )
}

#[derive(Clone)]
struct SkillRead(SkillSet);

#[async_trait]
impl Capability for SkillRead {
    fn name(&self) -> &str {
        "skill_read"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "skill_read",
            "Load a skill's instructions or a referenced text file on demand.",
            json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Skill name from the system prompt catalog." },
                    "path": { "type": "string", "description": "Relative file inside the skill folder. Defaults to SKILL.md." }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        )
    }
    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return Err(json!({"error": "skill_read requires string `name`"}));
        };
        let Some(skill) = self.0.skills.get(name) else {
            return Err(json!({"error": format!("unknown skill: {name}")}));
        };
        let rel = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or("SKILL.md");
        let rel_path = Path::new(rel);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err(
                json!({"error": "skill path must be a relative file path without traversal"}),
            );
        }
        let path = skill.root.join(rel_path);
        let canonical = path
            .canonicalize()
            .map_err(|e| json!({"error": format!("reading skill file {rel}: {e}")}))?;
        if !canonical.starts_with(&skill.root) || !canonical.is_file() {
            return Err(json!({"error": "skill path escapes its folder or is not a file"}));
        }
        let metadata = fs::metadata(&canonical).map_err(|e| json!({"error": e.to_string()}))?;
        if metadata.len() > MAX_SKILL_FILE_BYTES {
            return Err(json!({"error": "skill file exceeds 1 MB"}));
        }
        let content = fs::read_to_string(&canonical)
            .map_err(|e| json!({"error": format!("skill file is not UTF-8: {e}")}))?;
        Ok(json!({"name": name, "path": rel, "content": content}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "hugr-skills-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let skill = root.join("policy-review");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(skill.join("references")).unwrap();
        fs::write(skill.join("SKILL.md"), "---\nname: policy-review\ndescription: Review policy questions. Use for policy checks.\n---\n\n# Review\n\nRead references/rules.md.\n").unwrap();
        fs::write(skill.join("references/rules.md"), "Only verified claims.\n").unwrap();
        root
    }

    #[test]
    fn discovers_catalog_without_loading_skill_bodies() {
        let root = fixture();
        let set = discover_skills(std::slice::from_ref(&root)).unwrap();
        let prompt = skills_prompt("Base", &set);
        assert!(prompt.contains("`policy-review`: Review policy questions"));
        assert!(!prompt.contains("Only verified claims"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_frontmatter_name_mismatch() {
        let root = fixture();
        fs::write(
            root.join("policy-review/SKILL.md"),
            "---\nname: wrong\ndescription: Wrong name.\n---\n",
        )
        .unwrap();
        assert!(matches!(
            discover_skills(std::slice::from_ref(&root)),
            Err(SkillError::Invalid { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
