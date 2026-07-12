//! Typed response contract for the checked-in `huglet-docs` agent.

use huggr_agent::{AnswerHook, STATUS_SUCCESS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RESPONSE_RUST_TYPE: &str = "huglet_docs::DocsResponse";
pub const MODEL_RESPONSE_RUST_TYPE: &str = "huglet_docs::DocsModelResponse";

const HF_DOCS_BASE: &str = "https://huggingface.co/docs";

/// Public response payload returned by the docs agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents enriched with public Hugging Face documentation URLs.
    pub related_documents: Vec<Document>,
}

/// One source document cited by the docs agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    /// Source path relative to the runtime docs root, excluding AI_INDEX.md.
    pub path: String,
    /// Public URL for this document on huggingface.co/docs.
    pub url: String,
}

/// Model-facing response payload. The model cites paths only; the final answer
/// hook deterministically derives URLs after the response casts successfully.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsModelResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents relative to the runtime docs root, excluding AI_INDEX.md.
    pub related_documents: Vec<String>,
}

pub fn answer_hooks() -> Vec<AnswerHook> {
    vec![AnswerHook::new("huglet_docs::document_urls", |answer| {
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
        for document in related {
            if let Some(path) = document.as_str().map(str::to_string) {
                let url = document_url(&path);
                *document = json!({
                    "path": path,
                    "url": url,
                });
            } else if let Some(object) = document.as_object_mut() {
                let Some(path) = object
                    .get("path")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };
                object
                    .entry("url".to_string())
                    .or_insert_with(|| Value::String(document_url(&path)));
            }
        }
    })]
}

fn document_url(path: &str) -> String {
    let normalized = path.trim_start_matches('/');
    let slug = normalized.strip_suffix(".md").unwrap_or(normalized);
    format!("{HF_DOCS_BASE}/{slug}")
}
