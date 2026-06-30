//! # baton-host — the default native host
//!
//! `baton-host` is the **environment-specific** layer that drives the sans-IO
//! [`baton_core::Brain`]: it performs all IO, runs concurrency on tokio, and
//! turns the brain's [`Command`](baton_core::Command)s into real effects
//! (model calls, shell, fs, http), feeding results back as
//! [`Event`](baton_core::Event)s.
//!
//! The integration surface is small (DESIGN §8):
//!
//! ```no_run
//! use baton_host::{Engine, capabilities, policy::Interactive};
//! use baton_core::ModelSelector;
//! # async fn run(adapter: std::sync::Arc<dyn baton_host::ModelAdapter>) -> anyhow::Result<()> {
//! let mut engine = Engine::builder()
//!     .model(ModelSelector::named("big"), adapter)
//!     .capability(std::sync::Arc::new(capabilities::Shell))
//!     .capability(std::sync::Arc::new(capabilities::FsRead))
//!     .capability(std::sync::Arc::new(capabilities::FsWrite))
//!     .policy(std::sync::Arc::new(Interactive))
//!     .build();
//! engine.user_turn("list the rust files".into()).await;
//! # Ok(()) }
//! ```
//!
//! What lives here (host concerns, never in the core): the [`Capability`] and
//! [`ModelAdapter`] traits and their registries, the permission [`Policy`], the
//! [`Frontend`], and the tokio [`Engine`] driver loop.

pub mod capabilities;
mod capability;
mod engine;
mod frontend;
mod model;
pub mod policy;

pub use capability::{Capability, CapabilityRegistry, ChunkSink};
pub use engine::{Clock, Engine, EngineBuilder};
pub use frontend::{Frontend, Metrics, StdoutFrontend};
pub use model::{ModelAdapter, ModelRegistry, ModelSink};
pub use policy::Policy;
