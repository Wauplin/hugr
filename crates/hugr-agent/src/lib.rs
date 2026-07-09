//! `hugr-agent` — the common subagent runtime (ARCHITECTURE §18–19).
//!
//! This crate turns "an engine + a trace dir + a config" into a callable
//! subagent with a uniform contract: [`Ask`] in, [`Answer`] out. It is an
//! **internal layer**: the supported way to assemble an [`Agent`] is a
//! definition folder through `hugr-toolkit`'s `build_agent` (`hugr run` / a
//! built binary), and the supported calling surfaces are the CLI JSON
//! contract and `--mcp-serve`. The Rust API here is the shared implementation
//! those surfaces serialize, not a user-facing entry point.
//!
//! Contract design rules (ARCHITECTURE §18.1):
//!
//! - [`AnswerMeta`] is **mandatory** — an orchestrator can always account for
//!   a call.
//! - Errors are answers (`status: Error`, exit 0 on the CLI) so callers branch
//!   on data, not on exceptions.
//! - The user-facing payload rides in `Answer.response`; typed response
//!   contracts can generate provider JSON Schema and cast the final JSON into a
//!   Rust serde type before it is returned.

mod agent;
mod agent_tool;
mod blobs;
mod contract;
mod limits;
mod scratch;
mod store;

pub use agent::{
    Agent, AgentCard, AgentLimits, AskError, ModelTierCard, Pricing, ResponseContract, TierPrice,
    ToolCard,
};
pub use agent_tool::{AgentToolResolver, AgentToolSpec, depth_exceeded_resolver};
pub use blobs::BlobError;
pub use contract::{
    Answer, AnswerMeta, Ask, BlobHandle, BlobRef, STATUS_ERROR, STATUS_SUCCESS, TraceId,
};
pub use store::{StoreError, TraceHead, TraceHeader, TraceStore};

/// The content-addressed blob store outbound blobs land in (ARCHITECTURE
/// §18.3), re-exported from `hugr-replay` so orchestrators can resolve an
/// [`Answer`] blob's `sha256` ref via [`Agent::blob_store`].
pub use hugr_replay::BlobStore;
