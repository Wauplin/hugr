//! # baton-plugin-abi — the plugin ABI
//!
//! Third parties extend Baton with new tools **without recompiling the core**
//! (ROADMAP Phase 5). A plugin provides one or more capabilities and, optionally,
//! reacts to a narrow event view — exactly the `Capability`/policy model, just
//! loaded at runtime and sandboxed.
//!
//! ## The narrow contract (ARCHITECTURE §8.1)
//!
//! Three verbs carried as JSON — [`Request`]/[`Response`] in [`protocol`]:
//!
//! ```text
//! plugin exports:
//!   describe()          -> [ToolSchema]        // what capabilities it provides
//!   invoke(name, args)  -> stream<chunk> + result
//!   on_event(view)      -> [ ]                 // optional, NARROW reactions (reserved)
//! ```
//!
//! Every payload is an opaque [`Value`](baton_core::Value): adding a tool or an
//! argument changes **zero** core types (the narrow-waist rule, §2.4). The
//! contract is **versioned** ([`PROTOCOL_VERSION`]) so it can evolve without
//! breaking existing plugins, and a plugin can never touch core internals — it
//! only answers messages.
//!
//! ## Transports (where the plugin runs)
//!
//! The host depends only on the [`PluginTransport`] trait, so *where* a plugin
//! runs is pluggable:
//!
//! - [`SubprocessPlugin`] (always available) — the plugin is an external program;
//!   messages travel over stdio. Language-agnostic, process-sandboxed, separate
//!   repo, no core recompile. This is the roadmap's "secondary subprocess/MCP
//!   adapter path" and the working default.
//! - [`WasmPlugin`] (behind the `wasm` feature) — the roadmap's **primary** ABI:
//!   a sandboxed WASM component speaking the same protocol. Scaffolded here; the
//!   wasmtime backend lands with Phase 4 (portability). The trait seam means it
//!   drops in with no host changes.
//!
//! ## Where IO lives
//!
//! Like `baton-replay`, this is a **host-side** crate: it may spawn processes and
//! do IO. It uses `baton-core` only as pure data (`ToolSchema`/`Value`), so it
//! never pulls IO into the sans-IO core.

mod protocol;
mod subprocess;
mod transport;

pub use protocol::{PROTOCOL_VERSION, Request, Response};
pub use subprocess::SubprocessPlugin;
pub use transport::{PluginError, PluginSink, PluginTransport};

#[cfg(feature = "wasm")]
mod wasm;
#[cfg(feature = "wasm")]
pub use wasm::WasmPlugin;
