//! MCP stdio client adapted into ordinary host capabilities.
//!
//! This is deliberately a host-only integration: MCP servers are subprocesses,
//! their tools are advertised as [`Capability`](crate::Capability)s, and the
//! brain only ever sees normal `StartCapability` commands. Tool arguments and
//! results stay opaque `Value`s.

use std::ffi::OsString;
use std::process::Stdio;
use std::sync::Arc;

use crate::framing::{self, FramingError};
use async_trait::async_trait;
use huggr_core::{ToolSchema, Value};
use serde_json::json;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::capability::{Capability, ChunkSink};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// A stdio MCP server configuration.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct McpServerConfig {
    pub name: String,
    pub program: OsString,
    pub args: Vec<OsString>,
}

impl McpServerConfig {
    pub fn new(name: impl Into<String>, program: impl Into<OsString>) -> Self {
        Self {
            name: name.into(),
            program: program.into(),
            args: Vec::new(),
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl Into<OsString>>) -> Self {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
}

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum McpError {
    #[error("failed to spawn MCP server: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid MCP JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("MCP protocol error: {0}")]
    Protocol(String),
}

impl From<FramingError> for McpError {
    fn from(err: FramingError) -> Self {
        // Preserve the pre-framing error taxonomy: stream failures were `Io`,
        // malformed JSON was `Json`.
        match err {
            FramingError::Io(e) => McpError::Io(e),
            FramingError::Json(e) => McpError::Json(e),
        }
    }
}

/// One MCP tool exposed through the host capability interface.
pub struct McpToolCapability {
    client: Arc<McpClient>,
    schema: ToolSchema,
    remote_name: String,
}

impl McpToolCapability {
    fn new(client: Arc<McpClient>, schema: ToolSchema, remote_name: String) -> Self {
        Self {
            client,
            schema,
            remote_name,
        }
    }
}

#[async_trait]
impl Capability for McpToolCapability {
    fn name(&self) -> &str {
        &self.schema.name
    }

    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    async fn invoke(&self, args: Value, _sink: &ChunkSink) -> Result<Value, Value> {
        match self.client.call_tool(&self.remote_name, args).await {
            Ok(result) => {
                if result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    Err(result)
                } else {
                    Ok(result)
                }
            }
            Err(err) => Err(json!({
                "error": "mcp_transport",
                "message": err.to_string(),
            })),
        }
    }
}

struct McpClient {
    server_name: String,
    inner: Mutex<McpConnection>,
}

struct McpConnection {
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    next_id: u64,
    _child: Child,
}

impl McpClient {
    async fn connect(config: McpServerConfig) -> Result<Arc<Self>, McpError> {
        let mut child = Command::new(&config.program)
            .args(&config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Protocol("MCP server stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Protocol("MCP server stdout unavailable".into()))?;
        let client = Arc::new(Self {
            server_name: config.name,
            inner: Mutex::new(McpConnection {
                stdin,
                stdout: BufReader::new(stdout).lines(),
                next_id: 1,
                _child: child,
            }),
        });
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> Result<(), McpError> {
        let params = json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": "huggr", "version": env!("CARGO_PKG_VERSION") },
        });
        let _ = self.request("initialize", params).await?;
        self.notify("notifications/initialized", json!({})).await
    }

    async fn list_tools(&self) -> Result<Vec<McpTool>, McpError> {
        let result = self.request("tools/list", json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| McpError::Protocol("tools/list response missing tools[]".into()))?;
        tools
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(McpError::Json)
    }

    async fn call_tool(&self, name: &str, arguments: Value) -> Result<Value, McpError> {
        self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        let message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        framing::write_json_line(&mut inner.stdin, &message).await?;
        loop {
            let response: Value = framing::read_json_line(&mut inner.stdout)
                .await?
                .ok_or_else(|| McpError::Protocol("MCP server closed stdout".into()))?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(McpError::Protocol(error.to_string()));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| McpError::Protocol("MCP response missing result".into()));
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let mut inner = self.inner.lock().await;
        let message = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        framing::write_json_line(&mut inner.stdin, &message)
            .await
            .map_err(McpError::from)
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Value,
}

impl McpTool {
    fn schema(&self, server_name: &str) -> ToolSchema {
        ToolSchema::new(
            namespaced_tool_name(server_name, &self.name),
            self.description.clone().unwrap_or_default(),
            if self.input_schema.is_null() {
                json!({ "type": "object" })
            } else {
                self.input_schema.clone()
            },
        )
    }
}

/// Connect to an MCP stdio server and return its tools as ordinary
/// capabilities ready to register on [`EngineBuilder`](crate::EngineBuilder).
pub async fn load_stdio(config: McpServerConfig) -> Result<Vec<Arc<dyn Capability>>, McpError> {
    let client = McpClient::connect(config).await?;
    let tools = client.list_tools().await?;
    Ok(tools
        .into_iter()
        .map(|tool| {
            let schema = tool.schema(&client.server_name);
            Arc::new(McpToolCapability::new(client.clone(), schema, tool.name))
                as Arc<dyn Capability>
        })
        .collect())
}

fn namespaced_tool_name(server_name: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        sanitize_name(server_name),
        sanitize_name(tool_name)
    )
}

fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "server".to_string()
    } else {
        out
    }
}
