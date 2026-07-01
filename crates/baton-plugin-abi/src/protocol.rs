//! The wire protocol between the host and a plugin (ARCHITECTURE §8.1).
//!
//! Deliberately **narrow** (§8.1: "Narrow now, widen later") and **versioned**:
//! a host and a plugin agree on an integer [`PROTOCOL_VERSION`]. The contract is
//! three verbs — `describe` / `invoke` / `on_event` — carried as JSON. Because
//! every payload rides in an opaque [`Value`], adding a tool or an argument never
//! changes a core type (the narrow-waist rule, §2.4). It is transport-agnostic:
//! the same messages travel over stdio (the subprocess transport) or, later, a
//! WASM component call.
//!
//! A plugin receives a [`Request`] and answers with one or more [`Response`]s: an
//! `invoke` may stream any number of [`Response::Chunk`]s before its terminal
//! [`Response::Result`] or [`Response::Error`]; `describe` answers with exactly
//! one [`Response::Description`].

use baton_core::{ToolSchema, Value};
use serde::{Deserialize, Serialize};

/// The plugin ABI version the host speaks. A plugin reports the version it
/// implements in [`Response::Description`]; the host refuses a newer, unknown
/// one rather than mis-parsing (forward-compat, mirroring the trace format).
pub const PROTOCOL_VERSION: u32 = 1;

/// A message from the host to a plugin. `#[serde(tag = "req")]` so it is a tagged
/// JSON object a plugin in any language can match on (e.g. `{"req":"describe"}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "req", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Request {
    /// "What tools do you provide?" — answered by [`Response::Description`].
    Describe,
    /// "Run this tool." — the plugin streams chunks then a terminal result/error.
    Invoke {
        /// The tool name (one of those the plugin `describe`d).
        name: String,
        /// Opaque arguments (the model's tool-call args), forwarded verbatim.
        args: Value,
    },
    /// A **narrow** event notification (§8.1). Reserved: the host does not yet
    /// deliver these (it is wired in a later phase). Plugins may ignore it.
    OnEvent {
        /// An opaque, read-only *view* of what happened — never core internals.
        event: Value,
    },
}

/// A message from a plugin back to the host. `#[serde(tag = "kind")]` so it is a
/// tagged JSON object (e.g. `{"kind":"result","value":{…}}`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Response {
    /// The answer to [`Request::Describe`]: the plugin's protocol version and the
    /// tools it provides (each becomes a host `Capability`).
    Description {
        protocol_version: u32,
        tools: Vec<ToolSchema>,
    },
    /// A streamed chunk during an `invoke` (transport only, like a line of
    /// stdout) — forwarded to the brain as a `CapabilityChunk`.
    Chunk { value: Value },
    /// The terminal, successful result of an `invoke`.
    Result { value: Value },
    /// A terminal **semantic** error of an `invoke` (§5.4): a tool that ran but
    /// failed logically. Routed back to the model as an error tool-result.
    Error { value: Value },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The wire shape is a stable, language-agnostic tagged object — pin it so a
    /// plugin author in any language can rely on it.
    #[test]
    fn request_wire_shape_is_tagged() {
        assert_eq!(
            serde_json::to_value(Request::Describe).unwrap(),
            json!({ "req": "describe" })
        );
        assert_eq!(
            serde_json::to_value(Request::Invoke {
                name: "uppercase".into(),
                args: json!({ "text": "hi" }),
            })
            .unwrap(),
            json!({ "req": "invoke", "name": "uppercase", "args": { "text": "hi" } })
        );
    }

    #[test]
    fn response_round_trips() {
        for resp in [
            Response::Description {
                protocol_version: PROTOCOL_VERSION,
                tools: vec![ToolSchema::new("t", "d", json!({ "type": "object" }))],
            },
            Response::Chunk {
                value: json!({ "progress": 1 }),
            },
            Response::Result {
                value: json!({ "text": "HI" }),
            },
            Response::Error {
                value: json!({ "error": "boom" }),
            },
        ] {
            let bytes = serde_json::to_vec(&resp).unwrap();
            let back: Response = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(resp, back);
        }
    }

    /// A plugin author writes raw JSON (no baton deps) — it must decode.
    #[test]
    fn decodes_handwritten_result() {
        let line = r#"{"kind":"result","value":{"text":"HELLO"}}"#;
        assert_eq!(
            serde_json::from_str::<Response>(line).unwrap(),
            Response::Result {
                value: json!({ "text": "HELLO" })
            }
        );
    }
}
