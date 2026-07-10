use serde::{Deserialize, Serialize};

pub const DEFAULT_BASE_URL: &str = "https://router.huggingface.co/v1";
pub const DEFAULT_MODEL: &str = "google/gemma-4-31B-it:cerebras";

/// Host-provided agent configuration. The crate bakes nothing in: the system
/// prompt and provider settings are passed by the embedding host (an extension,
/// a web page, a node script) at construction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserAgentConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub max_model_calls: u32,
    pub max_cost_micro_usd: u64,
    /// The system prompt for the session; empty means "no system block".
    #[serde(default)]
    pub system_prompt: String,
}

impl Default for BrowserAgentConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: String::new(),
            max_model_calls: 2000,
            max_cost_micro_usd: 500_000,
            system_prompt: String::new(),
        }
    }
}
