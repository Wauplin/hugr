//! Events: the host → brain half of the contract.
//!
//! An [`Event`] is something that happened, fed into the brain's inbox. The
//! host merges many concurrent sources (model stream, shell, user, timers) into
//! one ordered, sequence-stamped stream; the brain reduces them one at a time,
//! atomically. `#[non_exhaustive]` so new variants don't break hosts.

use serde::{Deserialize, Serialize};

use crate::model::{ModelDelta, ModelOutput, Usage};
use crate::primitives::{OpId, Timestamp, Value};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Event {
    /// New conversational input. May arrive **at any time**, including mid-turn;
    /// the reducer consults `mode` (ARCHITECTURE §4.6). `content` is opaque/rich.
    UserInput {
        content: Value,
        mode: SteerMode,
        /// Host-provided approximate token count for the durable user message.
        #[serde(default)]
        est_tokens: u32,
    },
    /// Pure control signal: cancel current activity, no new content (e.g. ESC).
    UserAbort,

    // --- model streaming (transport only; never logged) ---------------------
    ModelDelta {
        op: OpId,
        delta: ModelDelta,
    },
    /// The authoritative, consolidated result. The brain's logic keys off this.
    ModelDone {
        op: OpId,
        output: ModelOutput,
        usage: Usage,
        /// Host/provider-provided approximate token count for the durable
        /// assistant message. The brain stores it and never tokenizes.
        #[serde(default)]
        est_tokens: u32,
    },
    ModelError {
        op: OpId,
        error: Value,
    },

    // --- capability results --------------------------------------------------
    /// A streamed chunk (transport only), e.g. a line of stdout.
    CapabilityChunk {
        op: OpId,
        chunk: Value,
    },
    /// A capability finished.
    CapabilityDone {
        op: OpId,
        result: Value,
        /// Host-provided approximate token count for the durable tool result.
        #[serde(default)]
        est_tokens: u32,
    },
    /// A capability failed.
    CapabilityError {
        op: OpId,
        error: Value,
        /// Host-provided approximate token count for the durable tool error.
        #[serde(default)]
        est_tokens: u32,
    },

    // --- brain asks ----------------------------------------------------------
    PermissionDecision {
        op: OpId,
        decision: Decision,
        /// Host-provided approximate token count for a denied permission result
        /// routed back to the model. `Allow` produces no durable content.
        #[serde(default)]
        est_tokens: u32,
    },

    /// An op the host aborted (in response to a [`Cancel`](crate::Command::Cancel),
    /// or externally).
    OpCancelled {
        op: OpId,
    },

    // --- injected nondeterminism --------------------------------------------
    /// Injected time. The brain stamps log entries with the latest `now`.
    Tick {
        now: Timestamp,
    },
}

/// How conversational input is handled when it arrives mid-turn (ARCHITECTURE
/// §4.6). The default is [`Queue`](SteerMode::Queue) (interrupt is reversible;
/// an accidental interrupt would discard in-flight work).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum SteerMode {
    /// Append the input; process it at the next turn boundary. Non-disruptive.
    #[default]
    Queue,
    /// Cancel in-flight ops, then start a fresh turn that sees both the partial
    /// work and the new instruction.
    Interrupt,
    /// Add to context; the current op finishes and the next model call sees it.
    AppendAndContinue,
}

/// A policy's decision on a permission request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Decision {
    Allow,
    Deny { reason: String },
}
