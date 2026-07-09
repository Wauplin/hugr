//! Typed response contract for the checked-in `hugr-docs` agent.

use hugr_agent::{AnswerHook, STATUS_SUCCESS};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsResponse";
pub const MODEL_RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsModelResponse";

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
pub struct  {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents relative to the runtime docs root, excluding AI_INDEX.md.
    pub related_documents: Vec<String>,
}

pub fn answer_hooks() -> Vec<AnswerHook> {
    vec![AnswerHook::new("hugr_docs::document_urls", |answer| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_agent::{Answer, TraceId};

    #[test]
    fn answer_hook_adds_hugging_face_doc_urls() {
        let mut answer = Answer {
            status: STATUS_SUCCESS.to_string(),
            response: json!({
                "response": "Use advanced compute options.",
                "related_documents": ["hub/advanced-compute-options.md"],
            }),
            trace_id: TraceId::new("trace"),
            ..Answer::default()
        };

        for hook in answer_hooks() {
            hook.apply(&mut answer);
        }

        assert_eq!(
            answer.response["related_documents"],
            json!([{
                "path": "hub/advanced-compute-options.md",
                "url": "https://huggingface.co/docs/hub/advanced-compute-options",
            }])
        );
    }
}
