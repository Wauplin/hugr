//! Rust response contract and extension point for the `huglet-weather` Huggr agent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const RESPONSE_RUST_TYPE: &str = "huglet_weather::Response";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub response: String,
}
