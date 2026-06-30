//! The `http` capability: a minimal HTTP request tool.

use async_trait::async_trait;
use baton_core::{ToolSchema, Value};
use serde_json::json;

use crate::capability::{Capability, ChunkSink};

/// Performs an HTTP request (default `GET`) and returns the status and body.
pub struct Http {
    client: reqwest::Client,
}

impl Http {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Capability for Http {
    fn name(&self) -> &str {
        "http"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "http",
            "Make an HTTP request and return the status code and response body.",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The URL to request." },
                    "method": { "type": "string", "description": "HTTP method (default GET)." },
                    "body": { "type": "string", "description": "Optional request body." }
                },
                "required": ["url"]
            }),
        )
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| json!({ "error": "missing string argument `url`" }))?;
        let method = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_uppercase();

        let req_method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|e| json!({ "error": format!("invalid method {method}: {e}") }))?;
        let mut req = self.client.request(req_method, url);
        if let Some(body) = args.get("body").and_then(Value::as_str) {
            req = req.body(body.to_string());
        }

        let resp = req
            .send()
            .await
            .map_err(|e| json!({ "error": format!("request failed: {e}") }))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| json!({ "error": format!("failed to read body: {e}") }))?;

        Ok(json!({ "status": status, "body": body }))
    }
}
