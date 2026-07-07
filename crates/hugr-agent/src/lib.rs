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
//! - `extra` is the narrow-waist escape hatch: agent-specific structure rides
//!   there, never as new contract fields.

mod agent;
mod agent_tool;
mod answer_schema;
mod blobs;
mod contract;
mod limits;
mod scratch;
mod store;

pub use agent::{
    Agent, AgentCard, AgentConfig, AgentLimits, AskError, ConfigEntry, ConfigProvenance,
    GroupBinding, GroupCapabilityFactory, ModelTierCard, Pricing, TierPrice, ToolCard,
    ToolPrivilege,
};
pub use agent_tool::{AgentToolResolver, AgentToolSpec, depth_exceeded_resolver};
pub use answer_schema::validate_extra;
pub use blobs::BlobError;
pub use contract::{
    Access, Answer, AnswerMeta, AnswerStatus, Ask, BlobHandle, BlobPerms, BlobRef, ResourceGrant,
    ResourceGroup, ResourceRef, TierSpend, TraceId,
};
pub use limits::LimitKind;
pub use store::{
    PrunePolicy, PruneReport, StoreError, StoreSize, TraceHead, TraceHeader, TraceStore,
};

/// The content-addressed blob store outbound blobs land in (ARCHITECTURE
/// §18.3), re-exported from `hugr-replay` so orchestrators can resolve an
/// [`Answer`] blob's `sha256` ref via [`Agent::blob_store`].
pub use hugr_replay::BlobStore;
