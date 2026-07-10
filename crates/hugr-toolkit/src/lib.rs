//! `hugr-toolkit` — Hugr subagent crate manifests.
//!
//! A subagent is a config folder, not a Rust project: a [`hugr.toml` manifest][manifest] plus a `SYSTEM.md` system prompt. This crate parses that folder into a typed [`AgentDefinition`], wires the predefined tool library, and drives the `hugr` builder CLI (`run`/`new`/`build`/`traces`).
//!
//! [manifest]: crate::manifest

pub mod build;
pub mod build_python;
pub mod bundle;
pub mod manifest;
pub mod mcp_serve;
pub mod runtime;
pub mod runtime_args;
pub mod scaffold;
pub mod schema_py;
pub mod stats;
pub mod surface;
pub mod tools;
pub mod traces;

pub use manifest::{
    AgentDefinition, AgentMeta, LimitsConfig, ManifestError, ModelsConfig, ResponseConfig,
    RuntimeArg, RuntimeConfig, ScratchpadConfig, TierConfig, ToolGrant, ToolKind, TracesConfig,
};
