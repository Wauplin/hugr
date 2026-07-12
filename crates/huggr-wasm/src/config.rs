use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
    /// Optional per-session cap on model calls; unset means unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<u32>,
    /// Optional per-session cost cap in micro-USD; unset means unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
    /// The system prompt for the session; empty means "no system block".
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default)]
    pub context: BrowserContextConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserContextConfig {
    #[serde(default = "default_context_compaction")]
    pub compaction: String,
    #[serde(default = "default_budget_tokens")]
    pub budget_tokens: u32,
    #[serde(default = "default_trigger_tokens")]
    pub trigger_tokens: u32,
    #[serde(default = "default_keep_recent_tokens")]
    pub keep_recent_tokens: u32,
    #[serde(default = "default_max_block_tokens")]
    pub max_block_tokens: u32,
    #[serde(default)]
    pub summary_model: Option<String>,
    #[serde(default)]
    pub tool_ttl: BTreeMap<String, u32>,
    #[serde(default = "default_keep_last_per_tool")]
    pub keep_last_per_tool: BTreeMap<String, u32>,
}

impl Default for BrowserAgentConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            api_key: String::new(),
            max_model_calls: None,
            max_cost_micro_usd: None,
            system_prompt: String::new(),
            context: BrowserContextConfig::default(),
        }
    }
}

impl Default for BrowserContextConfig {
    fn default() -> Self {
        Self {
            compaction: default_context_compaction(),
            budget_tokens: default_budget_tokens(),
            trigger_tokens: default_trigger_tokens(),
            keep_recent_tokens: default_keep_recent_tokens(),
            max_block_tokens: default_max_block_tokens(),
            summary_model: None,
            tool_ttl: BTreeMap::new(),
            keep_last_per_tool: default_keep_last_per_tool(),
        }
    }
}

fn default_context_compaction() -> String {
    "summarize".to_string()
}

fn default_budget_tokens() -> u32 {
    64_000
}

fn default_trigger_tokens() -> u32 {
    56_000
}

fn default_keep_recent_tokens() -> u32 {
    8_000
}

fn default_max_block_tokens() -> u32 {
    2_000
}

fn default_keep_last_per_tool() -> BTreeMap<String, u32> {
    BTreeMap::from([
        ("page_snapshot".to_string(), 1),
        ("page_read_text".to_string(), 1),
        ("page_read_html".to_string(), 1),
    ])
}
