//! The tokio driver loop (ARCHITECTURE §2.3) and its builder.
//!
//! The driver is the *entire* integration surface: drain `brain.poll()`,
//! perform each command (spawning one task per in-flight op), await the next
//! event from any source, `brain.submit()` it, repeat. All concurrency lives
//! here; the brain stays synchronous and single-threaded.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use hugr_core::{
    Brain, Command, ContextPlan, Decision, Event, ModelRequest, ModelSelector, OpId,
    SamplingParams, StaticPolicy, Timestamp, Value,
};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use hugr_replay::{Trace, TraceError, policy_from_trace};

use crate::ChunkSink;
use crate::capability::CapabilityRegistry;
use crate::frontend::Frontend;
use crate::model::{ModelRegistry, ModelSink};

/// Captures the exact ordered [`Event`] stream the host feeds the brain **and**
/// the ordered [`Command`] sequence the brain emits, so the session can be
/// persisted as a [`Trace`] and replayed bit-for-bit later (ARCHITECTURE
/// §6.2/§12). It records the *input* (events, including the injected `Tick`s) in
/// submission order and the *output* (commands) in the order the driver drains
/// them from `brain.poll()`; the durable *log* is read from the brain at save
/// time (it is always a fold over these same events).
///
/// The recorded commands let [`hugr_replay::verify`] assert that re-feeding the
/// events reproduces the same command sequence bit-for-bit — command *ordering*
/// never reaches the log, so without this a divergence (e.g. a `HashMap`-ordered
/// cancel-all) would pass verification undetected (§6.3).
///
/// Recording is opt-in (`Engine::builder().record()`); a non-recording engine
/// pays nothing.
#[derive(Clone, Debug, Default)]
pub(crate) struct Recorder {
    pub(crate) events: Vec<Event>,
    /// The brain→host commands, in the order the driver drained them from
    /// `brain.poll()` (the replay *output* verified against replay, §6.3).
    pub(crate) commands: Vec<Command>,
    /// The first injected timestamp, used as the trace's `created_at` (the
    /// `seq 0` tick — never a syscall in the core).
    pub(crate) created_at: Option<u64>,
}

impl Recorder {
    /// Record one event in submission order. The first `Tick` seeds `created_at`.
    pub(crate) fn record(&mut self, event: &Event) {
        if self.created_at.is_none() {
            if let Event::Tick { now } = event {
                self.created_at = Some(now.0);
            }
        }
        self.events.push(event.clone());
    }

    /// Record commands in the order the driver drained them from the brain, so
    /// the trace's command sequence matches what replay re-emits (§6.3).
    pub(crate) fn record_commands(&mut self, commands: &[Command]) {
        self.commands.extend_from_slice(commands);
    }

    /// Pre-load the recorder with a trace's already-recorded events **and** the
    /// commands re-derived by re-folding them, so a **resumed** session re-saves
    /// the full history (old + new) and still verifies bit-for-bit. The events
    /// are copied verbatim (including the recorded `Tick`s); `commands` come
    /// from the resume re-fold (see [`hugr_replay::drive`]) rather than the old
    /// trace's `commands`, so a resumed *old* trace (empty commands) still gets
    /// a complete, self-consistent command sequence. `created_at` is taken from
    /// the trace's metadata so the resumed trace keeps the original session's
    /// creation time, not a new one.
    fn seed(events: Vec<Event>, commands: Vec<Command>, created_at: Option<u64>) -> Self {
        Self {
            events,
            commands,
            created_at,
        }
    }
}

/// A source of (host-side) wall-clock time, injected into the brain as `Tick`
/// events so the brain itself never reads a clock (ARCHITECTURE §6.1).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// A handle for injecting [`Event`]s into a running [`Engine`] from outside a
/// turn (see [`Engine::event_sender`]). Cloneable and `Send`, so it can live in
/// a signal handler or another task. The classic use is `UserAbort`.
#[derive(Clone)]
pub struct EventSender {
    tx: UnboundedSender<Event>,
}

impl EventSender {
    /// Inject one event into the engine's inbox. Returns `false` if the engine
    /// has already shut down (its receiver dropped).
    pub fn send(&self, event: Event) -> bool {
        self.tx.send(event).is_ok()
    }

    /// Convenience: inject [`Event::UserAbort`] (cancel all in-flight work).
    pub fn abort(&self) -> bool {
        self.send(Event::UserAbort)
    }
}

/// The default front-end: renders nothing. The subagent runtime's product is
/// its `Answer` + trace, not a live render (ARCHITECTURE §18).
struct SilentFrontend;

impl Frontend for SilentFrontend {}

/// Drives a [`Brain`] against real IO on tokio. Build one with
/// [`Engine::builder`].
pub struct Engine {
    brain: Brain,
    models: ModelRegistry,
    caps: CapabilityRegistry,
    frontend: Box<dyn Frontend>,
    clock: Clock,
    tx: UnboundedSender<Event>,
    rx: UnboundedReceiver<Event>,
    tasks: HashMap<OpId, JoinHandle<()>>,
    /// Capability name per in-flight op, so tool results can be labelled when
    /// the engine observes their completion events.
    op_labels: HashMap<OpId, String>,
    /// When recording is enabled, the captured event stream for the trace
    /// (ARCHITECTURE §12). `None` when recording is off (zero overhead).
    recorder: Option<Recorder>,
    /// The brain's policy config, serialized once at build time, so a recorded
    /// trace can carry it (the brain branches on the policy's pure decisions —
    /// permission/background — so replay needs the same policy, §6.3).
    policy_config: Option<serde_json::Value>,
}

impl Engine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// Submit one conversational user message and drive the resulting turn (and
    /// any tool round-trips) to completion.
    pub async fn user_turn(&mut self, text: String) {
        self.submit(Event::UserInput {
            content: json!(text),
            est_tokens: estimate_text_tokens(&text),
        });
        self.drive_to_idle().await;
    }

    /// Read-only access to the underlying brain (log, op table, …).
    pub fn brain(&self) -> &Brain {
        &self.brain
    }

    /// Inspect the exact context plan the brain would use for the next normal
    /// model call, without mutating state or starting a turn.
    pub fn context_plan(&self) -> ContextPlan {
        self.brain.context_plan()
    }

    /// A cloneable handle for injecting [`Event`]s into the running loop from
    /// *outside* a turn — e.g. a Ctrl-C / signal handler sending
    /// [`Event::UserAbort`] while [`user_turn`](Self::user_turn) is awaiting the
    /// model stream. The event lands in the same inbox the per-op tasks feed, so
    /// the brain folds it in order (ARCHITECTURE §4.2): `UserAbort` makes the
    /// brain emit a [`Command::Cancel`] per in-flight op, the loop aborts those
    /// tasks, and the turn ends `Cancelled`.
    pub fn event_sender(&self) -> EventSender {
        EventSender {
            tx: self.tx.clone(),
        }
    }

    /// Signal the front-end that the session is finishing, so it can render any
    /// accumulated totals. Call this once after the last turn of a one-shot
    /// run, or when an interactive session exits.
    pub fn session_end(&mut self) {
        self.frontend.on_session_end();
    }

    /// Feed an event in, stamping it with a fresh injected `Tick` first.
    ///
    /// Both events go through here in submission order, so this is the single
    /// chokepoint where the [`Recorder`] captures the exact stream that produced
    /// the session — the property replay depends on (ARCHITECTURE §6.2).
    fn submit(&mut self, event: Event) {
        let now = Timestamp((self.clock)());
        let tick = Event::Tick { now };
        if let Some(rec) = self.recorder.as_mut() {
            rec.record(&tick);
            rec.record(&event);
        }
        self.brain.submit(tick);
        self.brain.submit(event);
    }

    /// Build a [`Trace`] of the session so far (the captured event stream + the
    /// brain's current durable log), or `None` if recording was not enabled on
    /// this engine. The trace replays bit-for-bit through
    /// [`hugr_replay::verify`].
    pub fn trace(&self) -> Option<Trace> {
        let rec = self.recorder.as_ref()?;
        let mut trace = Trace::new(
            rec.events.clone(),
            self.brain.state().log().to_vec(),
            rec.created_at,
        )
        // Carry the recorded command sequence so replay can verify it
        // bit-for-bit (command ordering never reaches the log, §6.3).
        .with_commands(rec.commands.clone());
        // Capture the policy so replay reproduces the brain's permission /
        // background branching bit-for-bit (§6.3).
        if let Some(policy) = self.policy_config.clone() {
            trace = trace.with_policy(policy);
        }
        Some(trace)
    }

    /// Save the recorded session to `path` as a trace. Errors if recording was
    /// not enabled or the write fails.
    pub fn save_trace(&self, path: impl AsRef<std::path::Path>) -> Result<(), TraceError> {
        match self.trace() {
            Some(trace) => trace.save(path),
            None => Err(TraceError::Io(std::io::Error::other(
                "engine is not recording; build it with .record()",
            ))),
        }
    }

    /// Process commands and events until no operation is in flight (the turn is
    /// complete).
    async fn drive_to_idle(&mut self) {
        loop {
            // Drain and perform every queued command. Performing one may queue
            // more (e.g. a tool result resuming the model), so loop until empty.
            loop {
                let commands = self.brain.poll();
                if commands.is_empty() {
                    break;
                }
                // Record the drained commands (in order) before performing them,
                // so the trace's command sequence matches what a replay re-emits
                // (command ordering never reaches the log, §6.3).
                if let Some(rec) = self.recorder.as_mut() {
                    rec.record_commands(&commands);
                }
                for command in commands {
                    self.perform(command).await;
                }
            }

            // No ops in flight → the turn is done.
            if self.brain.state().inflight_len() == 0 {
                break;
            }

            // Otherwise block until any task produces the next event.
            match self.rx.recv().await {
                Some(event) => {
                    self.observe(&event);
                    self.submit(event);
                }
                None => break,
            }
        }
    }

    /// Report incoming events to the front-end for observability, before the
    /// brain folds them. (Commands are reported in [`perform`](Self::perform).)
    fn observe(&mut self, event: &Event) {
        match event {
            Event::ModelDone { op, usage, .. } => self.frontend.on_model_end(*op, usage),
            Event::CapabilityDone { op, result, .. } => {
                let name = self.op_labels.remove(op).unwrap_or_default();
                self.frontend.on_tool_end(*op, &name, result, false);
            }
            Event::CapabilityError { op, error, .. } => {
                let name = self.op_labels.remove(op).unwrap_or_default();
                self.frontend.on_tool_end(*op, &name, error, true);
            }
            _ => {}
        }
    }

    /// Perform a single command from the brain.
    async fn perform(&mut self, command: Command) {
        match command {
            Command::StartModelCall { op, model, request } => match self.models.get(&model) {
                Some(adapter) => {
                    self.frontend.on_model_start(op, &model);
                    let handle = tokio::spawn(run_model_op(adapter, op, request, self.tx.clone()));
                    self.tasks.insert(op, handle);
                }
                None => {
                    let _ = self.tx.send(missing_model_event(op, &model));
                }
            },

            Command::StartCapability { op, name, args } => match self.caps.get(&name) {
                Some(capability) => {
                    self.frontend.on_tool_start(op, &name, &args);
                    self.op_labels.insert(op, name.clone());
                    let handle =
                        tokio::spawn(run_capability_op(capability, op, args, self.tx.clone()));
                    self.tasks.insert(op, handle);
                }
                None => {
                    let _ = self.tx.send(unknown_capability_event(op, &name));
                }
            },

            // Library tools are ungated — the sandbox is registration
            // (ARCHITECTURE §20.1): only what the manifest grants exists at
            // all, so permission requests are always allowed. The decision
            // still flows through the brain as a recorded event.
            Command::RequestPermission { op, request } => {
                let decision = Decision::Allow;
                self.frontend.on_permission(&request.capability, &decision);
                let _ = self.tx.send(Event::PermissionDecision {
                    op,
                    decision,
                    est_tokens: 0,
                });
            }

            Command::Cancel { op } => {
                if let Some(handle) = self.tasks.remove(&op) {
                    handle.abort();
                }
                let _ = self.tx.send(Event::OpCancelled { op });
            }

            // Cosmetic output (streamed text, tool chunks) goes straight to the
            // front-end; the brain already folded every delta, so this affects
            // only what is drawn, never the log.
            Command::Emit(event) => self.frontend.on_output(&event),

            // The recorder captures the event stream at `submit` (so the trace
            // is always buildable on demand via `Engine::trace`); a checkpoint
            // just drops finished task handles so they don't accumulate.
            Command::Checkpoint => {
                self.tasks.retain(|_, h| !h.is_finished());
            }

            Command::Done { reason } => {
                self.frontend.on_done(&reason);
            }

            // Forward-compatible: a newer core may add commands this host
            // doesn't know about yet (ARCHITECTURE §2.4).
            other => self
                .frontend
                .on_notice(&format!("(unhandled command: {other:?})")),
        }
    }
}

fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Run one model op to completion, streaming deltas via the [`ModelSink`] and
/// sending the terminal [`Event::ModelDone`]/[`Event::ModelError`] into `tx`.
pub(crate) async fn run_model_op(
    adapter: Arc<dyn crate::ModelAdapter>,
    op: OpId,
    request: ModelRequest,
    tx: UnboundedSender<Event>,
) {
    let sink = ModelSink::new(op, tx.clone());
    let event = match adapter.call(request, &sink).await {
        Ok((output, usage)) => {
            let est_tokens = model_output_est_tokens(&output, &usage);
            Event::ModelDone {
                op,
                output,
                usage,
                est_tokens,
            }
        }
        Err(error) => Event::ModelError {
            op,
            error: json!({ "message": error.to_string() }),
        },
    };
    let _ = tx.send(event);
}

/// The error event for a model selector with no registered adapter.
pub(crate) fn missing_model_event(op: OpId, model: &ModelSelector) -> Event {
    Event::ModelError {
        op,
        error: json!({ "message": format!("no adapter for model {model:?}") }),
    }
}

/// Run one capability op to completion, streaming chunks via the [`ChunkSink`]
/// and sending the terminal [`Event::CapabilityDone`]/[`Event::CapabilityError`]
/// (including token estimates) into `tx`.
pub(crate) async fn run_capability_op(
    capability: Arc<dyn crate::Capability>,
    op: OpId,
    args: Value,
    tx: UnboundedSender<Event>,
) {
    let sink = ChunkSink::new(op, tx.clone());
    let event = match capability.invoke(args, &sink).await {
        Ok(result) => Event::CapabilityDone {
            op,
            est_tokens: estimate_value_tokens(&result),
            result,
        },
        Err(error) => Event::CapabilityError {
            op,
            est_tokens: estimate_value_tokens(&error),
            error,
        },
    };
    let _ = tx.send(event);
}

/// The error event for a capability name with no registration.
pub(crate) fn unknown_capability_event(op: OpId, name: &str) -> Event {
    let error = json!({ "error": format!("unknown capability: {name}") });
    Event::CapabilityError {
        op,
        est_tokens: estimate_value_tokens(&error),
        error,
    }
}

/// Rough token estimate for a piece of text (~4 bytes per token, min 1).
/// Public so embedders can size opaque payloads consistently with the host's
/// own estimates.
pub fn estimate_text_tokens(text: &str) -> u32 {
    let bytes = text.len() as u64;
    bytes.div_ceil(4).max(1).min(u32::MAX as u64) as u32
}

pub(crate) fn estimate_value_tokens(value: &Value) -> u32 {
    match value {
        Value::String(text) => estimate_text_tokens(text),
        other => estimate_text_tokens(&other.to_string()),
    }
}

pub(crate) fn model_output_est_tokens(
    output: &hugr_core::ModelOutput,
    usage: &hugr_core::Usage,
) -> u32 {
    if usage.output_tokens > 0 {
        return usage.output_tokens.min(u32::MAX as u64) as u32;
    }
    estimate_text_tokens(&output.text)
}

/// Builds an [`Engine`]: register models + capabilities, then `build()`. The
/// builder also assembles the brain's [`StaticPolicy`] from the registered
/// capabilities (their schemas become the advertised tools, and the ones that
/// require permission become the gated set), so the caller doesn't repeat that.
pub struct EngineBuilder {
    models: ModelRegistry,
    caps: CapabilityRegistry,
    frontend: Option<Box<dyn Frontend>>,
    clock: Option<Clock>,
    /// The selector explicitly chosen via [`default_model`](Self::default_model),
    /// if any. Tracked separately from `first_model` so an explicit choice that
    /// happens to equal the built-in fallback (e.g. `named("medium")`) is still
    /// honored and can never be stolen by a later registration.
    default_model: Option<ModelSelector>,
    /// The first selector registered via [`model`](Self::model) — the documented
    /// convenience fallback when no explicit default was set.
    first_model: Option<ModelSelector>,
    system_prompt: Option<String>,
    model_request_extra: Value,
    sampling: SamplingParams,
    record: bool,
    /// When set, the brain is pre-seeded by replaying this trace's recorded
    /// events into it (with zero IO), and the recorder is pre-loaded with those
    /// same events so the continued session re-saves the full history.
    resume: Option<Trace>,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self {
            models: ModelRegistry::new(),
            caps: CapabilityRegistry::new(),
            frontend: None,
            clock: None,
            default_model: None,
            first_model: None,
            system_prompt: None,
            model_request_extra: Value::Null,
            sampling: SamplingParams::default(),
            record: false,
            resume: None,
        }
    }
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a model adapter under a logical selector. The first registered
    /// selector also becomes the one the turn policy calls (unless overridden
    /// with [`default_model`](Self::default_model)).
    pub fn model(mut self, selector: ModelSelector, adapter: Arc<dyn crate::ModelAdapter>) -> Self {
        if self.first_model.is_none() {
            self.first_model = Some(selector.clone());
        }
        self.models.register(selector, adapter);
        self
    }

    /// Override which logical selector the turn policy calls each turn. An
    /// explicit choice always wins over the first-registered fallback, even
    /// when it equals the built-in default (`named("medium")`).
    pub fn default_model(mut self, selector: ModelSelector) -> Self {
        self.default_model = Some(selector);
        self
    }

    /// Register a capability (tool).
    pub fn capability(mut self, capability: Arc<dyn crate::Capability>) -> Self {
        self.caps.register(capability);
        self
    }

    /// Set the front-end (default: a silent no-op).
    pub fn frontend(mut self, frontend: Box<dyn Frontend>) -> Self {
        self.frontend = Some(frontend);
        self
    }

    /// Override the clock (default: system wall-clock in ms). Tests inject a
    /// deterministic counter here.
    pub fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Set the system prompt prepended to every projected request.
    pub fn system_prompt(mut self, system: impl Into<String>) -> Self {
        self.system_prompt = Some(system.into());
        self
    }

    /// Set provider-specific opaque extras attached to every model request.
    pub fn model_request_extra(mut self, extra: Value) -> Self {
        self.model_request_extra = extra;
        self
    }

    /// Set sampling parameters for every request.
    pub fn sampling(mut self, params: SamplingParams) -> Self {
        self.sampling = params;
        self
    }

    /// Record the session: capture the ordered event stream so it can be saved
    /// as a [`Trace`] ([`Engine::trace`]/[`Engine::save_trace`]) and replayed
    /// bit-for-bit (ARCHITECTURE §12). Off by default (zero overhead).
    pub fn record(mut self, record: bool) -> Self {
        self.record = record;
        self
    }

    /// Resume a session from a saved [`Trace`]. The built engine's brain is
    /// reconstructed by re-feeding the trace's recorded events into it (with
    /// **zero IO** — the host does *not* re-run the model/tools for events that
    /// already happened; it only re-folds them to rebuild `BrainState`, exactly
    /// as [`hugr_replay::replay`] does). New live turns then continue from that
    /// state.
    ///
    /// Resuming implies recording (so the continued session can be re-saved as a
    /// trace that still verifies bit-for-bit): the recorder is pre-loaded with
    /// the trace's events, and any new events are appended after them. The
    /// session's `TurnPolicy` is restored from the trace ([`policy_from_trace`])
    /// so the continued session branches identically; a trace without a captured
    /// policy falls back to the default.
    pub fn resume(mut self, trace: Trace) -> Self {
        self.record = true;
        self.resume = Some(trace);
        self
    }

    pub fn build(self) -> Engine {
        let clock = self.clock.unwrap_or_else(|| Arc::new(system_clock));
        // Resolve the default model: an explicit `default_model` wins, then the
        // first registered model (the documented convenience), then the
        // built-in `named("medium")`. Explicitness is tracked, never inferred
        // by comparing values, so an explicit `named("medium")` is honored.
        let default_model = self
            .default_model
            .or(self.first_model)
            .unwrap_or_else(|| ModelSelector::named("medium"));
        // The brain's policy and recorder depend on whether we are resuming a
        // trace. Resume restores the *recorded* policy (so the continued session
        // branches identically and re-verifies) and rebuilds the brain from the
        // trace; a fresh session assembles the policy from the registered
        // capabilities (§2.4).
        let (brain, recorder, policy_config) = match self.resume {
            Some(trace) => {
                // The brain runs under the trace's policy; carry the captured
                // value through verbatim so re-saving round-trips it bit-for-bit.
                let mut brain = Brain::new(policy_from_trace(&trace));
                let events = trace.events;
                // Reconstruct the resumed session's state with ZERO IO: re-fold
                // the recorded events into the fresh brain (this only rebuilds
                // `BrainState`, exactly like `hugr_replay::replay`, via the
                // shared `drive` fold). We *keep* the re-emitted commands
                // (rather than discard them) to seed the recorder's command
                // sequence: re-deriving them here makes the resumed trace's
                // `commands` self-consistent with its events even when the
                // original trace predates command recording (empty `commands`),
                // so the re-saved trace still verifies bit-for-bit.
                let resume_commands = hugr_replay::drive(&mut brain, &events);
                // Pre-seed the recorder with the same events (moved, not cloned)
                // and the re-derived commands so a later `save_trace` carries
                // old + new (ARCHITECTURE §6.3).
                let mut recorder = Recorder::seed(events, resume_commands, trace.meta.created_at);
                reconcile_crashed_ops(&mut brain, &mut recorder, &clock);
                (brain, Some(recorder), trace.policy)
            }
            None => {
                let mut policy = StaticPolicy::default()
                    .with_model(default_model.clone())
                    .with_tools(self.caps.schemas())
                    .with_permissioned(self.caps.permissioned_names())
                    .with_background(self.caps.background_names())
                    .with_params(self.sampling)
                    .with_extra(self.model_request_extra);
                if let Some(system) = self.system_prompt {
                    policy = policy.with_system_prompt(system);
                }
                // Serialize the policy once (for recorded traces) before it
                // moves into the brain; best-effort.
                let policy_config = self
                    .record
                    .then(|| serde_json::to_value(&policy).ok())
                    .flatten();
                let brain = Brain::new(Box::new(policy));
                (brain, self.record.then(Recorder::default), policy_config)
            }
        };

        let (tx, rx) = mpsc::unbounded_channel();
        Engine {
            brain,
            models: self.models,
            caps: self.caps,
            frontend: self.frontend.unwrap_or_else(|| Box::new(SilentFrontend)),
            clock,
            tx,
            rx,
            tasks: HashMap::new(),
            op_labels: HashMap::new(),
            recorder,
            policy_config,
        }
    }
}

/// Append recorded [`Event::OpCancelled`] events for ops that were in flight
/// when the previous process died, so the brain folds the cancellation exactly
/// as if the old host had confirmed an abort before exiting. Replay-safe and
/// conservative (ARCHITECTURE §15.1).
fn reconcile_crashed_ops(brain: &mut Brain, recorder: &mut Recorder, clock: &Clock) {
    let stale: Vec<OpId> = brain.state().inflight().keys().copied().collect();
    for op in stale {
        let tick = Event::Tick {
            now: Timestamp(clock()),
        };
        recorder.record(&tick);
        brain.submit(tick);

        let cancelled = Event::OpCancelled { op };
        recorder.record(&cancelled);
        brain.submit(cancelled);
    }
    // Folding the stale cancellations queues commands — typically a
    // `Done { reason: Cancelled }` for the pre-crash turn, or even a
    // `StartModelCall` if an interrupt was pending when the process died. The
    // pre-crash turn is over: a resumed engine must start quiescent, so drain
    // and discard those commands here. Otherwise they would fire at the start
    // of the next live turn — a spurious stale pre-crash model call racing the
    // new one. They ARE still recorded: replaying the trace re-emits them, so
    // the recorded command sequence must include them for `verify` to match
    // bit-for-bit — "drained, not performed" is a host choice invisible to the
    // pure fold.
    recorder.record_commands(&brain.poll());
}
