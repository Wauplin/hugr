//! `web_fetch` — a host-allowlisted, GET-only HTTP tool. Privilege class:
//! **network**. The scope is the host allowlist declared in the manifest:
//!
//! ```toml
//! [tools.web_fetch]
//! allow_hosts = ["api.example.com", "docs.rs"]
//! allow_methods = ["GET"]        # optional; GET-only by default
//! max_bytes = 1000000            # optional response cap
//! ```
//!
//! A request whose host is not on the allowlist, or whose method is not
//! granted, returns a semantic tool error (the model sees it) — never a
//! transport panic. With no `allow_hosts` the tool denies every request, so an
//! empty grant is fail-closed.
//!
//! ## Sandbox hardening
//!
//! Automatic redirects are **disabled**: `reqwest` otherwise follows up to 10
//! redirects, and the allowlist is only checked on the *initial* URL — so an
//! allowlisted host could `3xx`-redirect to an off-allowlist (or internal)
//! target and exfiltrate/SSRF past the jail. With redirects off, a `3xx`
//! response is returned to the model as-is; following it is a *new* `web_fetch`
//! call whose target is re-checked against the allowlist. Only `http`/`https`
//! schemes are accepted (no `file:`/`ftp:`), and userinfo tricks
//! (`https://allowed@evil.com`) resolve to the real host, which is what the
//! allowlist checks.

use anyhow::{Context, Result};
use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use hugr_host::{Capability, ChunkSink};
use serde_json::json;

const DEFAULT_MAX_BYTES: usize = 1_000_000;

/// A network-egress tool jailed to an allowlist of hosts + methods.
pub struct WebFetch {
    client: reqwest::Client,
    allow_hosts: Vec<String>,
    allow_methods: Vec<String>,
    max_bytes: usize,
    markdown: bool,
}

impl WebFetch {
    /// Build from a manifest `[tools.web_fetch]` config value.
    pub fn from_config(config: &Value) -> Result<Self> {
        let allow_hosts = config
            .get("allow_hosts")
            .and_then(Value::as_array)
            .map(|hosts| {
                hosts
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|h| h.trim().to_ascii_lowercase())
                    .filter(|h| !h.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let allow_methods = config
            .get("allow_methods")
            .and_then(Value::as_array)
            .map(|ms| {
                ms.iter()
                    .filter_map(Value::as_str)
                    .map(|m| m.trim().to_ascii_uppercase())
                    .filter(|m| !m.is_empty())
                    .collect::<Vec<_>>()
            })
            .filter(|ms: &Vec<String>| !ms.is_empty())
            .unwrap_or_else(|| vec!["GET".to_string()]);
        let max_bytes = config
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|b| b as usize)
            .unwrap_or(DEFAULT_MAX_BYTES);
        // Disable automatic redirects: the allowlist is checked per URL,
        // so a redirect to an off-allowlist host must not be followed silently.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building web_fetch client")?;
        Ok(Self {
            client,
            allow_hosts,
            allow_methods,
            max_bytes,
            markdown: config
                .get("markdown")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    fn host_allowed(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        self.allow_hosts.iter().any(|allowed| {
            // Exact host, or a subdomain of an allowlisted host.
            host == *allowed || host.ends_with(&format!(".{allowed}"))
        })
    }
}

#[async_trait]
impl Capability for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema::new(
            "web_fetch",
            "Fetch a URL over HTTP(S). Restricted to an allowlist of hosts and methods declared in the agent manifest (GET-only by default).",
            json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "The absolute http(s) URL to request." },
                    "method": { "type": "string", "description": "HTTP method (default GET). Must be on the manifest allowlist." },
                    "markdown": { "type": "boolean", "description": "Convert the returned HTML body to Markdown. Defaults to the manifest setting." }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        )
    }

    fn requires_permission(&self) -> bool {
        // The allowlist is the sandbox boundary, so no per-call gate.
        false
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> std::result::Result<Value, Value> {
        let result = self.fetch(args).await;
        result.map_err(|error| json!({ "error": error.to_string() }))
    }
}

impl WebFetch {
    async fn fetch(&self, args: Value) -> Result<Value> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .context("web_fetch requires string `url`")?;
        let method = args
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();

        if !self.allow_methods.contains(&method) {
            anyhow::bail!(
                "method {method} is not granted; allowed: {}",
                self.allow_methods.join(", ")
            );
        }

        let parsed = reqwest::Url::parse(url).with_context(|| format!("invalid url: {url}"))?;
        anyhow::ensure!(
            matches!(parsed.scheme(), "http" | "https"),
            "only http(s) urls are allowed"
        );
        let host = parsed.host_str().context("url has no host")?;
        if !self.host_allowed(host) {
            anyhow::bail!(
                "host {host} is not on the allowlist; allowed: {}",
                if self.allow_hosts.is_empty() {
                    "(none)".to_string()
                } else {
                    self.allow_hosts.join(", ")
                }
            );
        }

        let req_method = reqwest::Method::from_bytes(method.as_bytes())
            .with_context(|| format!("invalid method {method}"))?;
        let resp = self
            .client
            .request(req_method, parsed)
            .send()
            .await
            .context("request failed")?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.context("failed to read body")?;
        let truncated = bytes.len() > self.max_bytes;
        let slice = if truncated {
            &bytes[..self.max_bytes]
        } else {
            &bytes[..]
        };
        let body = String::from_utf8_lossy(slice).into_owned();
        let markdown = args
            .get("markdown")
            .and_then(Value::as_bool)
            .unwrap_or(self.markdown);
        let body = if markdown {
            html2md::parse_html(&body)
        } else {
            body
        };
        Ok(json!({
            "status": status,
            "bytes_returned": body.len(),
            "truncated": truncated,
            "body": body,
            "format": if markdown { "markdown" } else { "raw" },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_allowlist_matches_exact_and_subdomains() {
        let tool = WebFetch::from_config(&json!({ "allow_hosts": ["example.com"] })).unwrap();
        assert!(tool.host_allowed("example.com"));
        assert!(tool.host_allowed("api.example.com"));
        assert!(!tool.host_allowed("evil.com"));
        assert!(!tool.host_allowed("notexample.com"));
    }

    #[tokio::test]
    async fn rejects_disallowed_host_and_method_without_network() {
        let tool = WebFetch::from_config(&json!({ "allow_hosts": ["example.com"] })).unwrap();
        // Host not on the allowlist — fails before any request.
        let err = tool
            .fetch(json!({ "url": "https://evil.com/x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("allowlist"), "{err}");
        // Method not granted (GET-only by default).
        let err = tool
            .fetch(json!({ "url": "https://example.com/x", "method": "POST" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not granted"), "{err}");
    }

    #[test]
    fn empty_allowlist_is_fail_closed() {
        let tool = WebFetch::from_config(&json!({})).unwrap();
        assert!(!tool.host_allowed("example.com"));
        assert_eq!(tool.allow_methods, vec!["GET".to_string()]);
    }

    #[test]
    fn markdown_default_is_configurable() {
        let tool = WebFetch::from_config(&json!({ "markdown": true })).unwrap();
        assert!(tool.markdown);
        let converted = html2md::parse_html("<h1>Title</h1>");
        assert!(converted.contains("Title"));
        assert!(!converted.contains("<h1>"));
    }

    #[tokio::test]
    async fn userinfo_and_nonhttp_schemes_cannot_bypass_the_allowlist() {
        let tool = WebFetch::from_config(&json!({ "allow_hosts": ["example.com"] })).unwrap();

        // `https://example.com@evil.com/` — the real host is evil.com, so the
        // allowlist rejects it (no network touched).
        let err = tool
            .fetch(json!({ "url": "https://example.com@evil.com/" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("allowlist"), "{err}");

        // Non-http(s) schemes are refused up front (no file:// exfiltration).
        let err = tool
            .fetch(json!({ "url": "file:///etc/passwd" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http(s)"), "{err}");
    }

    #[test]
    fn a_bare_host_does_not_match_a_different_host_with_shared_suffix() {
        // Regression: subdomain matching must require a dot boundary, so
        // `notexample.com` never matches an `example.com` allowlist.
        let tool = WebFetch::from_config(&json!({ "allow_hosts": ["example.com"] })).unwrap();
        assert!(!tool.host_allowed("notexample.com"));
        assert!(!tool.host_allowed("example.com.evil.com"));
        assert!(tool.host_allowed("deep.sub.example.com"));
    }
}
