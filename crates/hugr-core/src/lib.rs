//! # hugr-core — the brain
//!
//! `hugr-core` is the **pure, sans-IO state machine** at the heart of Hugr.
//! It is a reducer: it consumes one ordered stream of [`Event`]s and produces a
//! stream of [`Command`]s for a host to perform. It does **no** IO — no sockets,
//! no filesystem, no clock, no async runtime, no model calls, no rendering.
//!
//! ```text
//!     host.submit(event)  ──▶  brain folds it into state, queues commands
//!     host.poll()         ◀──  drains the commands the brain wants performed
//! ```
//!
//! See `docs/DESIGN.md` and `docs/ARCHITECTURE.md` for the full rationale. The
//! short version of the contract:
//!
//! - **Durable state is an append-only [`log`](state::BrainState::log).** The
//!   in-flight op table and everything else is a *fold* over that log.
//! - **The model is not special** — it is one event source among many,
//!   correlated to its [`Command::StartModelCall`] by an [`OpId`].
//! - **All nondeterminism is injected** ([`Event::Tick`], model output, tool
//!   results, user input), so a recorded event stream replays bit-for-bit.
//! - **Strategy lives in a pluggable [`TurnPolicy`]**, not in the reducer.
//!
//! This crate has **no environmental dependencies** (only `serde`/`serde_json`,
//! which are pure data). That is what lets the same brain compile to WASM, link
//! into a Python/JS binding, or run on a server — only the host differs.
//!
//! ## Phase 0 scope
//!
//! This started as the Phase 0 deliverable (see `docs/ROADMAP.md`): the turn loop
//! (`user → model → tool → model → done`), the op table, a trivial pass-through
//! [`projection`](TurnPolicy::project_context), and deterministic replay. Later
//! phases added, still sans-IO: cancellation & background ops, and **forks**
//! ([`Brain::from_log`]). Blob stores remain host-side; resume lives in the
//! host (`hugr-replay`).

#![forbid(unsafe_code)]
// `hugr-core` aspires to be `#![no_std]`-friendly (ARCHITECTURE §10/§11). It is
// not there yet (it uses `std::collections::HashMap` and `serde_json`); tracked
// as a later-phase goal once the footprint targets are validated.

mod brain;
mod command;
mod event;
mod model;
mod policy;
mod primitives;
mod record;
mod state;

pub use brain::Brain;
pub use command::{Command, DoneReason, OutputEvent, PermissionRequest};
pub use event::{Decision, Event, SteerMode};
pub use model::{
    ContentPart, ContextBlock, ContextBudgetTotals, ContextCacheHint, ContextDisposition,
    ContextPlan, ContextPlanEntry, ContextSource, ModelDelta, ModelOutput, ModelRequest,
    ModelSelector, Role, SamplingParams, StopReason, TokenBudget, ToolCall, ToolSchema, Usage,
};
pub use policy::{StaticPolicy, TurnPolicy, decode_policy};
pub use primitives::{OpId, Seq, Timestamp, Value};
pub use record::{LogEntry, OpMeta, OpOutcome, Record};
pub use state::{BrainState, InflightOp, OpKind};
