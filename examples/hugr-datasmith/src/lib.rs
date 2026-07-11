//! Rust response contract for the `hugr-datasmith` Hugr agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESPONSE_RUST_TYPE: &str = "hugr_datasmith::QaDataset";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_round_trips_and_rejects_unknown_fields() {
        let dataset = QaDataset {
            items: vec![QaItem {
                question: "How do I resume a trace?".into(),
                expected_answer: "Pass --trace <id>; a new child trace is written.".into(),
                source_path: "tutorials/01-first-agent-cli.md".into(),
                difficulty: "basic".into(),
            }],
            coverage: "CLI basics".into(),
        };
        let json = serde_json::to_string(&dataset).unwrap();
        assert_eq!(serde_json::from_str::<QaDataset>(&json).unwrap(), dataset);

        let with_extra = json.replacen('{', "{\"surprise\":1,", 1);
        assert!(serde_json::from_str::<QaDataset>(&with_extra).is_err());
    }

    #[test]
    fn schema_requires_the_grounding_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(QaDataset)).unwrap();
        let text = schema.to_string();
        for field in [
            "items",
            "coverage",
            "expected_answer",
            "source_path",
            "difficulty",
        ] {
            assert!(text.contains(field), "schema is missing `{field}`: {text}");
        }
    }
}
