//! `hugr-toolkit` — declarative Hugr subagent definitions (ARCHITECTURE §20–21,
//! ROADMAP T1).
//!
//! A subagent is a config folder, not a Rust project: a [`hugr.toml`
//! manifest][manifest] plus a `SYSTEM.md` system prompt. This crate parses that
//! folder into a typed [`AgentDefinition`], wires the predefined tool library
//! (T1.2), and drives the `hugr` builder CLI (`run`/`new`/`build`/`traces`).
//!
//! T1.1 lands the manifest parser; later tasks stack the tool library and the
//! CLI on top. The crate is a *host* layer — it stacks on `hugr-agent` and never
//! reaches into `hugr-core` internals.
//!
//! [manifest]: crate::manifest

pub mod build;
pub mod bundle;
pub mod manifest;
pub mod mcp_serve;
pub mod runtime;
pub mod runtime_args;
pub mod scaffold;
pub mod surface;
pub mod tools;
pub mod traces;

pub use manifest::{
    AgentDefinition, AgentMeta, LimitsConfig, ManifestError, ModelsConfig, RuntimeArg,
    RuntimeConfig, ScratchpadConfig, TierConfig, ToolGrant, ToolKind, TracesConfig,
};
