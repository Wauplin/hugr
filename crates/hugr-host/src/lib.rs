//! # hugr-host — the default native host
//!
//! `hugr-host` is the **environment-specific** layer that drives the sans-IO
//! [`hugr_core::Brain`]: it performs all IO, runs concurrency on tokio, and
//! turns the brain's [`Command`](hugr_core::Command)s into real effects
//! (model calls, shell, fs, http), feeding results back as
//! [`Event`](hugr_core::Event)s.
//!
//! The integration surface is small (DESIGN §8):
//!
//! ```no_run
//! use hugr_host::{Engine, capabilities, policy::AllowAll};
//! use hugr_core::ModelSelector;
//! # async fn run(adapter: std::sync::Arc<dyn hugr_host::ModelAdapter>) -> anyhow::Result<()> {
//! let mut engine = Engine::builder()
//!     .model(ModelSelector::named("medium"), adapter)
//!     .capability(std::sync::Arc::new(capabilities::Shell))
//!     .capability(std::sync::Arc::new(capabilities::FsRead))
//!     .capability(std::sync::Arc::new(capabilities::FsWrite))
//!     .policy(std::sync::Arc::new(AllowAll))
//!     .build();
//! engine.user_turn("list the rust files".into()).await;
//! # Ok(()) }
//! ```
//!
//! What lives here (host concerns, never in the core): the [`Capability`] and
//! [`ModelAdapter`] traits and their registries, the permission [`Policy`], the
//! [`Frontend`], and the tokio [`Engine`] driver loop.

mod agent;
pub mod capabilities;
mod capability;
mod coalesce;
mod engine;
mod frontend;
mod model;
pub mod plugins;
pub mod policy;
mod scheduler;

pub use capability::{Capability, CapabilityRegistry, ChunkSink};
pub use engine::{
    CheckpointCadence, Clock, CrashResumePolicy, Engine, EngineBuilder, EventSender,
    TraceCompaction,
};
pub use frontend::{Frontend, Metrics, StdoutFrontend};
pub use model::{ModelAdapter, ModelRegistry, ModelSink};
pub use plugins::PluginCapability;
pub use policy::Policy;
pub use scheduler::{CronExpr, Schedule, ScheduleError, TriggerTarget, fire_once};

// Re-export the plugin ABI so a host embedder needs only `hugr-host` to load
// plugins (the ABI crate lives behind us, like `hugr-replay`).
pub use hugr_plugin_abi::{self, PluginError, PluginSink, PluginTransport, SubprocessPlugin};

// Re-export the trace + replay surface so a host embedder needs only one crate
// to record a session and replay it (the persistence crate lives behind us).
pub use hugr_replay::{self, Inspector, Replay, Step, Trace, TraceError};
