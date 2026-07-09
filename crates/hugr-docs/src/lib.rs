//! Typed response contract for the checked-in `hugr-docs` agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsResponse";

/// Final response payload produced by the docs agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsResponse {
    /// User-facing answer grounded in the retrieved documents.
    pub response: String,
    /// Source documents relative to the runtime docs root, excluding AI_INDEX.md.
    pub related_documents: Vec<String>,
}
