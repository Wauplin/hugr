//! Small shared primitives used across the contract.

use serde::{Deserialize, Serialize};

/// Identifies a single in-flight operation (a model call, a tool invocation, a
/// sub-agent, …). Carried on every effectful [`Command`](crate::Command) and
/// referenced by every [`Event`](crate::Event) that reports progress, so the
/// brain can correlate results and cancel work.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct OpId(pub u64);

impl std::fmt::Display for OpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "op:{}", self.0)
    }
}

/// Host-assigned global ordering of log entries. Also the replay key: replay
/// feeds recorded entries back in `seq` order.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Seq(pub u64);

/// A logical timestamp. The brain **never reads a clock**; time is injected via
/// [`Event::Tick`](crate::Event::Tick) and stamped onto log entries. The unit
/// is host-defined (milliseconds since epoch is the conventional choice).
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Timestamp(pub u64);

/// An opaque payload the brain stores and forwards but never interprets:
/// capability args/results, tool payloads, provider-specific knobs, prompts,
/// answers.
pub type Value = serde_json::Value;
