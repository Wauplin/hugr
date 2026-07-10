//! # Replay & inspection
//!
//! Re-feeding a trace's recorded [`Event`]s into a *fresh*
//! [`Brain`](hugr_core::Brain) reproduces every [`Command`] it ever emitted,
//! with no IO.
//!
//! - [`replay`] re-feeds the events and returns the reconstructed commands + log.
//! - [`verify`] does that and asserts the reconstruction equals the recording.
//! - [`Inspector`] wraps the same reconstruction so a debugger can step through
//!   the session one event at a time.

use hugr_core::{Brain, Command, Event, LogEntry, StaticPolicy, TurnPolicy, decode_policy};

use crate::{Trace, TraceError};

/// The result of replaying a trace's event stream through a fresh brain.
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
/// The brain branches on some of its [`TurnPolicy`]'s pure decisions
/// (`needs_permission`, `is_background`), so reconstructing the exact
/// command/log sequence requires the *same* policy, not just the recorded
/// events. If the trace captured its policy ([`Trace::with_policy`]), this
/// decodes it via [`decode_policy`]; otherwise it falls back to the default.
/// Use [`replay_with_policy`] to supply a custom one.
pub fn replay(trace: &Trace) -> Replay {
    replay_with_policy(trace, policy_from_trace(trace))
}

/// Reconstruct the [`TurnPolicy`] a trace was recorded under: decode the
/// captured [`StaticPolicy`] config if present (via [`decode_policy`]), else
/// the default.
///
/// This is the policy a faithful replay or resume must run under — the brain
/// branches on the policy's pure decisions, so continuing a session requires
/// the same policy the trace was recorded with.
pub fn policy_from_trace(trace: &Trace) -> Box<dyn TurnPolicy> {
    trace
        .policy
        .as_ref()
        .and_then(decode_policy)
        // No captured policy, or one we can't decode (e.g. a custom host
        // policy): fall back to the default rather than fail. The caller can
        // supply the right policy via `replay_with_policy`.
        .unwrap_or_else(|| Box::new(StaticPolicy::default()))
}

/// Fold an ordered event stream into `brain`, draining and returning every
/// [`Command`] it emits. Both replay (which keeps the commands) and the host's
/// resume path (which rebuilds state and discards them) drive a brain this
/// way, so the loop lives here once.
pub fn drive(brain: &mut Brain, events: &[Event]) -> Vec<Command> {
    let mut commands = Vec::new();
    for event in events {
        brain.submit(event.clone());
        commands.extend(brain.poll());
    }
    // A final drain in case the last event queued commands the loop above did
    // not pick up (it always polls after each submit, but be defensive).
    commands.extend(brain.poll());
    commands
}

/// Replay a trace through a fresh brain built with a specific [`TurnPolicy`].
pub fn replay_with_policy(trace: &Trace, policy: Box<dyn TurnPolicy>) -> Replay {
    let mut brain = Brain::new(policy);
    let commands = drive(&mut brain, &trace.events);
    Replay {
        commands,
        log: brain.state().log().to_vec(),
    }
}

/// Replay a trace and assert the reconstruction is **byte-identical** to the
/// recording. Returns the [`Replay`] on success.
///
/// Two things are checked:
///
/// - the reconstructed **command sequence** equals the trace's recorded
///   [`commands`](Trace::commands), in order — this catches command-order
///   nondeterminism (e.g. a `HashMap`-ordered cancel-all) that never reaches
///   the log at all;
/// - the reconstructed **consolidated log** equals the recorded log.
///
/// **Back-compat:** a trace whose `commands` is empty (an older recording made
/// before the command sequence was captured) is checked log-only. Either
/// mismatch means the fold is no longer deterministic for this trace — exactly
/// the regression replay exists to catch.
pub fn verify(trace: &Trace) -> Result<Replay, TraceError> {
    verify_with_policy(trace, policy_from_trace(trace))
}

/// [`verify`] with an explicit policy.
pub fn verify_with_policy(
    trace: &Trace,
    policy: Box<dyn TurnPolicy>,
) -> Result<Replay, TraceError> {
    let replay = replay_with_policy(trace, policy);
    check_replay(trace, &replay)?;
    Ok(replay)
}

/// Assert a reconstruction matches a trace's recorded commands and log.
///
/// The command comparison only runs when the trace actually captured a
/// sequence: an empty `commands` means the trace predates command recording,
/// so we fall back to the log-only check to keep verifying old traces.
fn check_replay(trace: &Trace, replay: &Replay) -> Result<(), TraceError> {
    if !trace.commands.is_empty() && replay.commands != trace.commands {
        let index = first_divergence(&trace.commands, &replay.commands);
        return Err(TraceError::CommandMismatch {
            index,
            recorded: trace.commands.len(),
            reconstructed: replay.commands.len(),
        });
    }
    if replay.log != trace.log {
        return Err(TraceError::ReplayMismatch {
            recorded: trace.log.len(),
            reconstructed: replay.log.len(),
        });
    }
    Ok(())
}

/// Index of the first position where two command slices differ (or the length
/// of the shorter one when they share a prefix but differ in length).
fn first_divergence(recorded: &[Command], reconstructed: &[Command]) -> usize {
    recorded
        .iter()
        .zip(reconstructed.iter())
        .position(|(a, b)| a != b)
        .unwrap_or_else(|| recorded.len().min(reconstructed.len()))
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

/// A step-through debugger over a trace. Construct it from a [`Trace`], then
/// call [`step`](Inspector::step) repeatedly: each call feeds the next recorded
/// event into the same fresh brain and reports exactly what that event
/// produced.
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
