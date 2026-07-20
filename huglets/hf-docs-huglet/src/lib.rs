//! Typed response contract for the Hugging Face documentation huglet.

use huggr_agent::{AnswerHook, STATUS_SUCCESS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RESPONSE_RUST_TYPE: &str = "hf_docs_huglet::DocsResponse";
pub const MODEL_RESPONSE_RUST_TYPE: &str = "hf_docs_huglet::DocsModelResponse";

const HF_DOCS_BASE: &str = "https://huggingface.co/docs";

/// Public response payload returned by the HF docs huglet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents enriched with public Hugging Face documentation URLs.
    pub related_documents: Vec<Document>,
}

/// One source document cited by the HF docs huglet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Source path relative to the runtime documentation root.
    pub path: String,
    /// Public URL for the corresponding Hugging Face documentation page.
    pub url: String,
}

/// Model-facing response payload. The final hook derives public URLs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsModelResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source paths relative to the runtime documentation root.
    pub related_documents: Vec<String>,
}

pub fn answer_hooks() -> Vec<AnswerHook> {
    vec![AnswerHook::new("hf_docs_huglet::document_urls", |answer| {
        if answer.status != STATUS_SUCCESS {
            return;
        }
        let Some(related) = answer
            .response
            .get_mut("related_documents")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        related.retain_mut(|document| {
            let Some(path) = document_path(document) else {
                return false;
            };
            if is_navigation_index(&path) {
                return false;
            }
            let url = document_url(&path);
            *document = json!({ "path": path, "url": url });
            true
        });
    })]
}

fn document_path(document: &Value) -> Option<String> {
    let path = document
        .as_str()
        .or_else(|| document.get("path").and_then(Value::as_str))?;
    let normalized = path
        .trim()
        .trim_start_matches('/')
        .strip_prefix("docs/")
        .unwrap_or_else(|| path.trim().trim_start_matches('/'));
    if normalized.is_empty() || normalized.split('/').any(|part| part == "..") {
        return None;
    }
    Some(normalized.to_string())
}

fn is_navigation_index(path: &str) -> bool {
    path.rsplit('/').next() == Some("AI_INDEX.md")
}

fn document_url(path: &str) -> String {
    let without_markdown = path.strip_suffix(".md").unwrap_or(path);
    let page = without_markdown
        .strip_suffix("/index")
        .unwrap_or(without_markdown);
    if page == "index" || page.is_empty() {
        HF_DOCS_BASE.to_string()
    } else {
        format!("{HF_DOCS_BASE}/{page}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use huggr_agent::Answer;

    #[test]
    fn answer_hook_adds_hugging_face_document_urls() {
        let mut answer = Answer {
            status: STATUS_SUCCESS.to_string(),
            response: json!({
                "response": "Rate limits depend on the service.",
                "related_documents": ["hub/rate-limits.md", "transformers/index.md"],
            }),
            ..Answer::default()
        };

        answer_hooks()[0].apply(&mut answer);

        assert_eq!(
            answer.response["related_documents"],
            json!([
                {
                    "path": "hub/rate-limits.md",
                    "url": "https://huggingface.co/docs/hub/rate-limits",
                },
                {
                    "path": "transformers/index.md",
                    "url": "https://huggingface.co/docs/transformers",
                }
            ])
        );
    }

    #[test]
    fn answer_hook_normalizes_paths_and_removes_navigation_indexes() {
        let mut answer = Answer {
            status: STATUS_SUCCESS.to_string(),
            response: json!({
                "response": "Use the Python client.",
                "related_documents": [
                    "docs/huggingface_hub/guides/download.md",
                    "huggingface_hub/AI_INDEX.md",
                    "../outside.md"
                ],
            }),
            ..Answer::default()
        };

        answer_hooks()[0].apply(&mut answer);

        assert_eq!(
            answer.response["related_documents"],
            json!([{
                "path": "huggingface_hub/guides/download.md",
                "url": "https://huggingface.co/docs/huggingface_hub/guides/download",
            }])
        );
    }
}
