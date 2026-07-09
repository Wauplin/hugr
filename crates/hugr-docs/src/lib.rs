//! Typed response contract for the checked-in `hugr-docs` definition.
//!
//! This crate intentionally does not define a custom CLI or runtime. It owns
//! the Rust response type and registers it with `hugr-toolkit`'s shared surface.

use std::time::Instant;

use hugr_toolkit::manifest::AgentDefinition;
use hugr_toolkit::runtime::RuntimeOptions;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsResponse";
pub const RESPONSE_SCHEMA_NAME: &str = "hugr_docs_response";

/// Final response payload produced by the docs agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents relative to the runtime docs root, excluding AI_INDEX.md.
    pub related_documents: Vec<String>,
}

/// JSON Schema generated from [`DocsResponse`].
pub fn response_schema() -> Value {
    runtime_options()
        .response_schema(RESPONSE_RUST_TYPE)
        .expect("DocsResponse is registered")
}

/// Runtime wiring for the shared toolkit surfaces.
pub fn runtime_options() -> RuntimeOptions {
    RuntimeOptions::new()
        .with_response_type::<DocsResponse>(RESPONSE_RUST_TYPE, RESPONSE_SCHEMA_NAME)
}

/// Shared-surface entrypoint for development runs of this checked-in example.
pub async fn main() -> i32 {
    let started = Instant::now();
    let definition_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("definition");
    match AgentDefinition::load(&definition_dir) {
        Ok(def) => {
            hugr_toolkit::surface::run_definition_args_with_options(
                def,
                std::env::args_os().skip(1),
                started,
                runtime_options(),
            )
            .await
        }
        Err(err) => hugr_toolkit::surface::print_answer(
            &hugr_toolkit::surface::error_answer(err.to_string(), started),
            true,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_schema_matches_the_wire_shape() {
        let schema = response_schema();
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["required"],
            serde_json::json!(["response", "related_documents"])
        );
        assert_eq!(schema["properties"]["response"]["type"], "string");
        assert_eq!(schema["properties"]["related_documents"]["type"], "array");
    }

    #[test]
    fn response_rejects_unknown_fields() {
        let err = serde_json::from_value::<DocsResponse>(serde_json::json!({
            "response": "Hugr is ...",
            "related_documents": [],
            "answer": "old field"
        }))
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }
}
