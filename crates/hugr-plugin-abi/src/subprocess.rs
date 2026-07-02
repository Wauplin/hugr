//! The subprocess transport: a plugin is an external program the host runs,
//! exchanging protocol JSON over stdio (ARCHITECTURE §8.2).
//!
//! This is the pragmatic, **language-agnostic** path (the roadmap's "secondary
//! subprocess/MCP adapter path"): a plugin can be written in any language, ships
//! as its own binary in its own repo, and needs **no recompile of the core** —
//! it depends on nothing from Hugr, only on the documented JSON protocol.
//! Isolation is process-level (the OS sandbox), aligning with the capability /
//! policy model.
//!
//! Each request spawns a fresh process that handles exactly one request then
//! exits. That keeps the transport **stateless and naturally concurrent** (no
//! shared pipe to multiplex, no interleaving of chunks between calls) at the cost
//! of a spawn per call — the right trade for correctness-first Phase 5.

use std::ffi::OsString;
use std::process::Stdio;

use async_trait::async_trait;
use hugr_core::{ToolSchema, Value};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

use crate::framing::{read_json_line, write_json_line};
use crate::protocol::{PROTOCOL_VERSION, Request, Response};
use crate::transport::{PluginError, PluginSink, PluginTransport};

/// A plugin backed by an external program spoken to over stdio.
#[derive(Clone, Debug)]
pub struct SubprocessPlugin {
    program: OsString,
    args: Vec<OsString>,
}

impl SubprocessPlugin {
    /// A plugin that runs `program` (looked up on `PATH` or an absolute path).
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Append a fixed argument passed to the plugin program on every spawn.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Spawn the plugin, send `request`, and read responses. Chunks are forwarded
    /// to `sink`; the terminal response is returned. `describe` and `invoke` share
    /// this exchange.
    async fn exchange(
        &self,
        request: &Request,
        sink: &PluginSink,
    ) -> Result<Response, PluginError> {
        let mut child = Command::new(&self.program)
            .args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;

        // Send the one request, then close stdin so the plugin sees EOF.
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| PluginError::Protocol("plugin stdin unavailable".into()))?;
            write_json_line(&mut stdin, request).await?;
            // `stdin` drops here → EOF for the plugin.
        }

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::Protocol("plugin stdout unavailable".into()))?;
        let mut lines = BufReader::new(stdout).lines();

        let mut terminal = None;
        while let Some(response) = read_json_line::<_, Response>(&mut lines).await? {
            match response {
                Response::Chunk { value } => sink.chunk(value),
                other => {
                    terminal = Some(other);
                    break;
                }
            }
        }

        // Reap the child (don't leave a zombie); ignore its exit status — the
        // protocol response is authoritative.
        let _ = child.wait().await;

        terminal.ok_or_else(|| {
            PluginError::Protocol("plugin closed without a terminal response".into())
        })
    }
}

#[async_trait]
impl PluginTransport for SubprocessPlugin {
    async fn describe(&self) -> Result<Vec<ToolSchema>, PluginError> {
        match self
            .exchange(&Request::Describe, &PluginSink::null())
            .await?
        {
            Response::Description {
                protocol_version,
                tools,
            } => {
                if protocol_version > PROTOCOL_VERSION {
                    return Err(PluginError::UnsupportedVersion {
                        found: protocol_version,
                        supported: PROTOCOL_VERSION,
                    });
                }
                Ok(tools)
            }
            Response::Error { value } => Err(PluginError::Protocol(value.to_string())),
            other => Err(PluginError::Protocol(format!(
                "expected a description, got {other:?}"
            ))),
        }
    }

    async fn invoke(&self, name: &str, args: Value, sink: &PluginSink) -> Result<Value, Value> {
        let request = Request::Invoke {
            name: name.to_string(),
            args,
        };
        match self.exchange(&request, sink).await {
            Ok(Response::Result { value }) => Ok(value),
            Ok(Response::Error { value }) => Err(value),
            Ok(other) => Err(json!({
                "error": "plugin_protocol",
                "message": format!("expected a result, got {other:?}"),
            })),
            // Transport failures become a semantic error the model can react to,
            // rather than crashing the turn (§5.4).
            Err(err) => Err(json!({
                "error": "plugin_error",
                "message": err.to_string(),
            })),
        }
    }
}
