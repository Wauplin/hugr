//! `hugr-agent` — the common subagent runtime API (ARCHITECTURE §18–19).
//!
//! This crate turns "an engine + a trace dir + a config" into a callable
//! subagent with a uniform contract: [`Ask`] in, [`Answer`] out. One Rust API
//! is the source of truth; every wire shape (CLI JSON, Python dict, MCP tool
//! result) is a serialization of it.
//!
//! Contract design rules (ARCHITECTURE §18.1):
//!
//! - [`AnswerMeta`] is **mandatory** — an orchestrator can always account for
//!   a call.
//! - Errors are answers (`status: Error`, exit 0 on the CLI) so callers branch
//!   on data, not on exceptions.
//! - `extra` is the narrow-waist escape hatch: agent-specific structure rides
//!   there, never as new contract fields.
//! - Every public type is `#[non_exhaustive]` with constructors, so the
//!   contract can grow without breaking hosts or surfaces.

mod agent;
mod blobs;
mod contract;
mod scratch;
mod store;

pub use agent::{
    Agent, AgentBuilder, AgentCard, AgentConfig, AgentLimits, AskError, ConfigEntry,
    ConfigProvenance, ModelTierCard, Pricing, TierPrice, ToolCard, ToolPrivilege,
};
pub use blobs::BlobError;
pub use contract::{
    Answer, AnswerMeta, AnswerStatus, Ask, BlobHandle, BlobPerms, BlobRef, TierSpend, TraceId,
};
pub use store::{StoreError, TraceHead, TraceHeader, TraceStore};

/// The content-addressed blob store outbound blobs land in (ARCHITECTURE
/// §18.3), re-exported from `hugr-replay` so orchestrators can resolve an
/// [`Answer`] blob's `sha256` ref via [`Agent::blob_store`].
pub use hugr_replay::BlobStore;
