//! Host-side enforcement of the manifest `[limits]` (ROADMAP T3.1,
//! ARCHITECTURE §18/§20.1).
//!
//! Enforcement lives entirely in the host layer — `hugr-core` never learns
//! about limits, so the sans-IO brain and its deterministic replay are
//! untouched. Two mechanisms cover the declared [`AgentLimits`](crate::AgentLimits):
//!
//! - **Counting / cost limits** (`max_model_calls`, `max_turns`,
//!   `max_cost_micro_usd`) are enforced by wrapping every registered model
//!   adapter in a [`LimitedAdapter`]. Before each model call it checks the
//!   shared [`LimitState`]; once a bound is crossed it refuses the call and
//!   returns an error, which the brain folds into `ModelError` → `Done(Error)`
//!   and ends the turn. The refusal is an ordinary recorded event, so the
//!   partial trace still replays bit-for-bit (`verify()` never re-calls the
//!   adapter — it re-feeds the recorded `ModelError`).
//! - **The wall-clock `timeout_ms` limit** is enforced in the ask path by
//!   wrapping the turn in `tokio::time::timeout`; on elapse the trip is
//!   recorded and the (partial) trace is persisted as it stands.
//!
//! Either way the ask returns a normal [`Answer`](crate::Answer) with
//! `status: Error`, a typed reason in `Answer.extra` (`{"limit_exceeded": …}`),
//! and a persisted `trace_id` — exceeding a limit is an *answer*, not an
//! `AskError` (§18.1).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use hugr_core::{ModelOutput, ModelRequest, Usage};
use hugr_host::{ModelAdapter, ModelSink};
use serde_json::{Value, json};

use crate::agent::{AgentLimits, Pricing};

/// Which declared limit stopped an ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum LimitKind {
    /// `[limits].max_model_calls` — total model calls in the ask.
    MaxModelCalls,
    /// `[limits].max_turns` — model round-trips (turn steps) in the ask.
    MaxTurns,
    /// `[limits].max_cost_micro_usd` — accumulated cost across model calls.
    MaxCostMicroUsd,
    /// `[limits].timeout_s` — wall-clock duration of the ask.
    Timeout,
}

impl LimitKind {
    /// The stable machine-readable key used in `Answer.extra`.
    pub fn as_str(self) -> &'static str {
        match self {
            LimitKind::MaxModelCalls => "max_model_calls",
            LimitKind::MaxTurns => "max_turns",
            LimitKind::MaxCostMicroUsd => "max_cost_micro_usd",
            LimitKind::Timeout => "timeout_ms",
        }
    }
}

/// A recorded limit trip: which bound was crossed and the numeric value it was
/// set to. Produced by [`LimitState`] and consumed by the ask path to shape the
/// error [`Answer`](crate::Answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LimitTrip {
    pub kind: LimitKind,
    pub limit: u64,
}

impl LimitTrip {
    /// The human-readable message for the error answer.
    pub(crate) fn message(&self) -> String {
        match self.kind {
            LimitKind::MaxModelCalls => {
                format!("limit exceeded: max_model_calls ({} model calls)", self.limit)
            }
            LimitKind::MaxTurns => format!("limit exceeded: max_turns ({} turns)", self.limit),
            LimitKind::MaxCostMicroUsd => {
                format!(
                    "limit exceeded: max_cost_micro_usd ({} micro-USD)",
                    self.limit
                )
            }
            LimitKind::Timeout => format!("limit exceeded: timeout ({} ms)", self.limit),
        }
    }

    /// The typed, machine-readable reason placed on `Answer.extra`.
    pub(crate) fn extra(&self) -> Value {
        json!({
            "limit_exceeded": {
                "limit": self.kind.as_str(),
                "value": self.limit,
            }
        })
    }
}

/// Shared, per-ask enforcement state for the counting/cost limits, updated by
/// every [`LimitedAdapter`] wrapping the ask's model adapters. Cheaply shared
/// via `Arc`; all counters are atomic so concurrent model ops (background tools)
/// stay correct.
pub(crate) struct LimitState {
    limits: AgentLimits,
    pricing: Pricing,
    /// Model calls attempted this ask (incremented on every wrapped call,
    /// including a refused one — a refusal ends the turn, so it never races a
    /// following success).
    model_calls: AtomicU64,
    /// Accumulated cost so far, in micro-USD, folded from completed calls.
    cost_micro_usd: AtomicU64,
    /// The first trip observed (later trips are ignored — the first one already
    /// ends the turn).
    trip: Mutex<Option<LimitTrip>>,
}

impl LimitState {
    pub(crate) fn new(limits: AgentLimits, pricing: Pricing) -> Arc<Self> {
        Arc::new(Self {
            limits,
            pricing,
            model_calls: AtomicU64::new(0),
            cost_micro_usd: AtomicU64::new(0),
            trip: Mutex::new(None),
        })
    }

    /// True when a counting/cost limit is set, so the ask must wrap its model
    /// adapters. A timeout-only limit set doesn't need adapter wrapping.
    pub(crate) fn needs_adapter_wrap(&self) -> bool {
        self.limits.max_model_calls.is_some()
            || self.limits.max_turns.is_some()
            || self.limits.max_cost_micro_usd.is_some()
    }

    /// Record the first limit trip observed.
    fn record_trip(&self, kind: LimitKind, limit: u64) {
        let mut guard = self.trip.lock().unwrap();
        if guard.is_none() {
            *guard = Some(LimitTrip { kind, limit });
        }
    }

    /// Record a wall-clock timeout trip from the ask path.
    pub(crate) fn record_timeout(&self, timeout_ms: u64) {
        self.record_trip(LimitKind::Timeout, timeout_ms);
    }

    /// The trip that stopped this ask, if any.
    pub(crate) fn trip(&self) -> Option<LimitTrip> {
        *self.trip.lock().unwrap()
    }

    /// Called before each wrapped model call. Returns the refusal error when a
    /// counting/cost bound has been crossed (recording the trip), else `None`.
    fn check_before_call(&self) -> Option<anyhow::Error> {
        let call_no = self.model_calls.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(max) = self.limits.max_model_calls {
            if call_no > max as u64 {
                self.record_trip(LimitKind::MaxModelCalls, max as u64);
                return Some(refusal(LimitKind::MaxModelCalls, max as u64));
            }
        }
        if let Some(max) = self.limits.max_turns {
            if call_no > max as u64 {
                self.record_trip(LimitKind::MaxTurns, max as u64);
                return Some(refusal(LimitKind::MaxTurns, max as u64));
            }
        }
        if let Some(max) = self.limits.max_cost_micro_usd {
            // Check the cost accumulated by *completed* calls: we cannot know a
            // call's cost until it returns usage, so a bound is enforced by
            // refusing the *next* call once the running total has crossed it.
            if self.cost_micro_usd.load(Ordering::SeqCst) >= max {
                self.record_trip(LimitKind::MaxCostMicroUsd, max);
                return Some(refusal(LimitKind::MaxCostMicroUsd, max));
            }
        }
        None
    }

    /// Fold a completed call's cost into the running total.
    fn record_cost(&self, selector: &str, usage: &Usage) {
        let cost = self
            .pricing
            .cost_micro_usd(selector, usage.input_tokens, usage.output_tokens);
        if cost > 0 {
            self.cost_micro_usd.fetch_add(cost, Ordering::SeqCst);
        }
    }
}

fn refusal(kind: LimitKind, limit: u64) -> anyhow::Error {
    anyhow::anyhow!(LimitTrip { kind, limit }.message())
}

/// A [`ModelAdapter`] that enforces the ask's counting/cost limits around a
/// wrapped adapter. Registered under the same selector as the adapter it wraps,
/// so the brain's model routing is unchanged.
pub(crate) struct LimitedAdapter {
    selector: String,
    inner: Arc<dyn ModelAdapter>,
    state: Arc<LimitState>,
}

impl LimitedAdapter {
    pub(crate) fn new(
        selector: String,
        inner: Arc<dyn ModelAdapter>,
        state: Arc<LimitState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            selector,
            inner,
            state,
        })
    }
}

#[async_trait]
impl ModelAdapter for LimitedAdapter {
    async fn call(
        &self,
        request: ModelRequest,
        sink: &ModelSink,
    ) -> anyhow::Result<(ModelOutput, Usage)> {
        if let Some(err) = self.state.check_before_call() {
            return Err(err);
        }
        let (output, usage) = self.inner.call(request, sink).await?;
        self.state.record_cost(&self.selector, &usage);
        Ok((output, usage))
    }
}
