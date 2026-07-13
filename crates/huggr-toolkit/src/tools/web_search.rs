use anyhow::{Context, Result};
use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use huggr_host::{Capability, ChunkSink};
use serde_json::json;

const DEFAULT_MAX_RESPONSE_BYTES: usize = 4_000_000;

/// Exa-backed web search configured with an environment-owned API key.
pub struct WebSearch {
    client: reqwest::Client,
    api_key_env: String,
    max_results: u64,
    max_bytes: usize,
}

impl WebSearch {
    /// Build a search capability from one `[tools.web_search]` grant.
    pub fn from_config(config: &Value) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            api_key_env: config
                .get("api_key_env")
                .and_then(Value::as_str)
                .unwrap_or("EXA_API_KEY")
                .to_string(),
            max_results: config
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 100),
            max_bytes: config
                .get("max_bytes")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_RESPONSE_BYTES),
        })
    }
    async fn search(&self, args: Value) -> Result<Value> {
        let query = args
            .get("query")
            .and_then(Value::as_str)
            .context("web_search requires string `query`")?;
        let num_results = args
            .get("num_results")
            .and_then(Value::as_u64)
            .unwrap_or(self.max_results)
            .clamp(1, self.max_results);
        let key = std::env::var(&self.api_key_env).with_context(|| {
            format!("web_search API key env var `{}` is unset", self.api_key_env)
        })?;
        let mut body = json!({"query":query,"numResults":num_results});
        if args
            .get("contents")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            body["contents"] = json!({"text":true});
        }
        let mut response = self
            .client
            .post("https://api.exa.ai/search")
            .header("x-api-key", key)
            .json(&body)
            .send()
            .await
            .context("Exa search request failed")?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("reading Exa search response")?
        {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= self.max_bytes,
                "Exa search response exceeded {} bytes; lower `num_results` or disable `contents`",
                self.max_bytes
            );
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).context("decoding Exa search response")?;
        anyhow::ensure!(status.is_success(), "Exa search returned {status}: {value}");
        Ok(value)
    }
}

#[async_trait]
impl Capability for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "web_search",
            "Search the web through Exa. Requires the operator-configured API key environment variable.",
            json!({"type":"object","properties":{"query":{"type":"string"},"num_results":{"type":"integer","minimum":1,"maximum":100},"contents":{"type":"boolean","description":"Include extracted page text. Defaults to false."}},"required":["query"],"additionalProperties":false}),
        )
    }
    fn requires_permission(&self) -> bool {
        false
    }
    async fn invoke(&self, args: Value, _: &ChunkSink) -> std::result::Result<Value, Value> {
        self.search(args)
            .await
            .map_err(|e| json!({"error":e.to_string()}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_key_fails_before_network_access() {
        let tool =
            WebSearch::from_config(&json!({"api_key_env":"HUGGR_TEST_EXA_KEY_THAT_IS_UNSET"}))
                .unwrap();
        let error = tool.search(json!({"query":"test"})).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("HUGGR_TEST_EXA_KEY_THAT_IS_UNSET")
        );
    }
}
