//! # Replay & inspection (ARCHITECTURE §6.3)
//!
//! Replay is the whole point of the sans-IO design: because the brain is a pure
//! fold over an ordered event stream, re-feeding a trace's recorded [`Event`]s
//! into a *fresh* [`Brain`](baton_core::Brain) reproduces every [`Command`] it
//! ever emitted — **bit-for-bit**, with no IO. The recorded `log` is the *truth*
//! a replay is checked against; `BrainState` is never stored, only rederived
//! (ARCHITECTURE §12.1).
//!
//! This module is host-side and pure-of-environment: it drives `baton-core` as
//! pure data (`submit`/`poll`), touching no clock, socket, or model. That is why
//! a trace recorded on a server replays identically in a browser or a test.
//!
//! Two entry points:
//!
//! - [`replay`] re-feeds the events and returns the reconstructed commands + log.
//! - [`verify`] does that and asserts the reconstructed log equals the recorded
//!   one — the Phase 3 exit criterion (record a session, replay it bit-for-bit).
//!
//! [`Inspector`] wraps the same reconstruction so a debugger can **step through**
//! the session one event at a time, watching the commands each event produced
//! and the log entries it appended.

use baton_core::{Brain, Command, Event, LogEntry, StaticPolicy, TurnPolicy};

use crate::{Trace, TraceError};

/// The result of replaying a trace's event stream through a fresh brain.
///
/// `commands` is the exact ordered sequence the brain emitted (the bit-for-bit
/// reconstruction); `log` is the durable log rebuilt by folding the same events.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Replay {
    /// Every command the brain emitted, in order, across the whole stream.
    pub commands: Vec<Command>,
    /// The reconstructed durable log (a fold over the replayed events).
    pub log: Vec<LogEntry>,
}

/// Replay a trace through a fresh [`Brain`], reproducing the session.
///
/// The brain's *strategy* lives in its [`TurnPolicy`], and the brain **branches**
/// on some of the policy's pure decisions (`needs_permission`, `is_background`)
/// — so reconstructing the exact command/log sequence requires the *same*
/// policy, not just the recorded events. If the trace captured its policy
/// ([`Trace::with_policy`]), this decodes it as a [`StaticPolicy`]; otherwise it
/// falls back to the default. Use [`replay_with_policy`] to supply a custom one.
pub fn replay(trace: &Trace) -> Replay {
    replay_with_policy(trace, policy_from_trace(trace))
}

/// Reconstruct the [`TurnPolicy`] a trace was recorded under: decode the
/// captured [`StaticPolicy`] config if present, else the default.
///
/// This is the policy a faithful replay (or **resume**, P3-4) must run under —
/// the brain branches on the policy's pure decisions, so continuing a session
/// requires the same policy the trace was recorded with. A trace with no
/// captured policy (or one we can't decode) falls back to the default.
pub fn policy_from_trace(trace: &Trace) -> Box<dyn TurnPolicy> {
    match &trace.policy {
        Some(value) => match serde_json::from_value::<StaticPolicy>(value.clone()) {
            Ok(policy) => Box::new(policy),
            // A policy we can't decode (e.g. a custom non-StaticPolicy host):
            // fall back to the default rather than fail. The caller can supply
            // the right policy via `replay_with_policy`.
            Err(_) => Box::new(StaticPolicy::default()),
        },
        None => Box::new(StaticPolicy::default()),
    }
}

/// Replay a trace through a fresh brain built with a specific [`TurnPolicy`].
pub fn replay_with_policy(trace: &Trace, policy: Box<dyn TurnPolicy>) -> Replay {
    let mut brain = Brain::new(policy);
    let mut commands = Vec::new();

    // Re-feed the exact ordered event stream and drain commands after each one —
    // mirroring the host driver loop, but with zero IO (ARCHITECTURE §2.3/§6.3).
    for event in &trace.events {
        brain.submit(event.clone());
        commands.extend(brain.poll());
    }
    // A final drain in case the last event queued commands the loop above did
    // not pick up (it always polls after each submit, but be defensive).
    commands.extend(brain.poll());

    Replay {
        commands,
        log: brain.state().log().to_vec(),
    }
}

/// Replay a trace and assert the reconstructed log is **byte-identical** to the
/// recorded log — the Phase 3 exit criterion (record a real session, replay it
/// bit-for-bit). Returns the [`Replay`] on success.
///
/// The check compares the *consolidated log* (the durable truth), not the raw
/// deltas (transport, never logged, §4.5). A mismatch means the fold is no
/// longer deterministic for this trace — exactly the regression replay exists to
/// catch.
pub fn verify(trace: &Trace) -> Result<Replay, TraceError> {
    verify_with_policy(trace, policy_from_trace(trace))
}

/// [`verify`] with an explicit policy.
pub fn verify_with_policy(
    trace: &Trace,
    policy: Box<dyn TurnPolicy>,
) -> Result<Replay, TraceError> {
    let replay = replay_with_policy(trace, policy);
    if replay.log != trace.log {
        return Err(TraceError::ReplayMismatch {
            recorded: trace.log.len(),
            reconstructed: replay.log.len(),
        });
    }
    Ok(replay)
}

/// One step of a replay: the event that was fed, the commands it produced, and
/// the log entries it appended. The unit an [`Inspector`] yields.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct Step {
    /// 0-based index of this event in the trace's `events` stream.
    pub index: usize,
    /// The event fed into the brain this step.
    pub event: Event,
    /// The commands the brain emitted *in response to this event*.
    pub commands: Vec<Command>,
    /// The log entries appended by this event (the new tail since the last step).
    pub appended: Vec<LogEntry>,
}

/// A step-through debugger over a trace (ARCHITECTURE §6.3, "step through a real
/// session deterministically"). Construct it from a [`Trace`], then call
/// [`step`](Inspector::step) repeatedly: each call feeds the next recorded event
/// into the same fresh brain and reports exactly what that event produced.
///
/// Host-side and pure: it drives `baton-core` as data, so a front-end (the CLI's
/// `--step` mode, a TUI, a notebook) can render the reconstruction live without
/// any IO of its own.
pub struct Inspector {
    brain: Brain,
    events: Vec<Event>,
    index: usize,
    /// Number of log entries already reported, so each step yields only the tail.
    reported_log: usize,
}

impl Inspector {
    /// An inspector over a trace, using the policy the trace captured (or the
    /// default [`StaticPolicy`] if none) — see [`replay`] for why the policy
    /// matters for faithful reconstruction.
    pub fn new(trace: &Trace) -> Self {
        Self::with_policy(trace, policy_from_trace(trace))
    }

    /// An inspector over a trace with an explicit [`TurnPolicy`].
    pub fn with_policy(trace: &Trace, policy: Box<dyn TurnPolicy>) -> Self {
        Self {
            brain: Brain::new(policy),
            events: trace.events.clone(),
            index: 0,
            reported_log: 0,
        }
    }

    /// Total number of recorded events (steps).
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether there are no recorded events.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// How many steps have been taken so far.
    pub fn position(&self) -> usize {
        self.index
    }

    /// Read-only access to the brain as reconstructed up to the current step.
    pub fn brain(&self) -> &Brain {
        &self.brain
    }

    /// Feed the next recorded event into the brain and report what it produced.
    /// Returns `None` once every event has been replayed.
    pub fn step(&mut self) -> Option<Step> {
        if self.index >= self.events.len() {
            return None;
        }
        let index = self.index;
        let event = self.events[index].clone();

        self.brain.submit(event.clone());
        let commands = self.brain.poll();

        let log = self.brain.state().log();
        let appended = log[self.reported_log..].to_vec();
        self.reported_log = log.len();

        self.index += 1;
        Some(Step {
            index,
            event,
            commands,
            appended,
        })
    }

    /// Run to the end, collecting every [`Step`]. Equivalent to calling
    /// [`step`](Inspector::step) until it returns `None`.
    pub fn run(mut self) -> Vec<Step> {
        let mut steps = Vec::with_capacity(self.events.len());
        while let Some(step) = self.step() {
            steps.push(step);
        }
        steps
    }
}
