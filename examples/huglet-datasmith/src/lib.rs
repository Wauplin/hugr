//! Rust response contract for the `huglet-datasmith` Huggr agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESPONSE_RUST_TYPE: &str = "huglet_datasmith::QaDataset";

/// The synthesized evaluation dataset: grounded Q&A pairs mined from a
/// documentation folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QaDataset {
    /// The generated question/answer pairs.
    pub items: Vec<QaItem>,
    /// One sentence naming the documentation areas the pairs span.
    pub coverage: String,
}

/// One grounded question/answer pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QaItem {
    /// A natural user question answerable from the docs.
    pub question: String,
    /// The expected answer, fully supported by the cited source file.
    pub expected_answer: String,
    /// Path of the supporting file, relative to the docs root.
    pub source_path: String,
    /// Open difficulty label: `basic`, `intermediate`, or `advanced`.
    pub difficulty: String,
}
