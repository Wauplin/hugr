//! Events: the host → brain half of the contract.
//!
//! An [`Event`] is something that happened, fed into the brain's inbox as an
//! [`Envelope`] stamped with the host's injected time. The host merges many
//! concurrent sources (model stream, shell, user, timers) into one ordered,
//! time-stamped stream; the brain reduces them one at a time, atomically.
//! `#[non_exhaustive]` so new variants don't break hosts.

use serde::{Deserialize, Serialize};

use crate::model::{ModelDelta, ModelOutput, Usage};
use crate::primitives::{OpId, Timestamp, Value};

/// One unit of brain input: an [`Event`] stamped with the host's injected
/// wall-clock time. The brain has no clock; `at` is its only source of time,
/// which keeps the fold pure and replay bit-for-bit deterministic.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    /// Host-injected time at submission, stamped onto everything this event
    /// makes durable (log entries, op start/end).
    pub at: Timestamp,
    pub event: Event,
}

impl Envelope {
    /// Stamp `event` with the host's injected time.
    pub fn new(at: Timestamp, event: Event) -> Self {
        Self { at, event }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Event {
    /// New conversational input. Hosts submit it only while the brain is idle;
    /// input received during an active turn is ignored. `content` is opaque/rich.
    UserInput {
        content: Value,
        /// Host-provided approximate token count for the durable user message.
        #[serde(default)]
        est_tokens: u32,
    },
    /// Pure control signal: cancel current activity, no new content (e.g. ESC).
    UserAbort,

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
}

/// A policy's decision on a permission request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Decision {
    Allow,
    Deny { reason: String },
}
