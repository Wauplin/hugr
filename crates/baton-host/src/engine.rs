//! The tokio driver loop (ARCHITECTURE §2.3) and its builder.
//!
//! The driver is the *entire* integration surface: drain `brain.poll()`,
//! perform each command (spawning one task per in-flight op), await the next
//! event from any source, `brain.submit()` it, repeat. All concurrency lives
//! here; the brain stays synchronous and single-threaded.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use baton_core::{
    Brain, Command, Event, ModelSelector, OpId, SamplingParams, StaticPolicy, SteerMode, Timestamp,
    Value,
};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use baton_replay::{Trace, TraceError, policy_from_trace};

use crate::ChunkSink;
use crate::capability::CapabilityRegistry;
use crate::coalesce::Coalescer;
use crate::frontend::{Frontend, StdoutFrontend};
use crate::model::{ModelRegistry, ModelSink};
use crate::policy::{AllowAll, Policy};

/// Captures the exact ordered [`Event`] stream the host feeds the brain, so the
/// session can be persisted as a [`Trace`] and replayed bit-for-bit later
/// (ARCHITECTURE §6.2/§12). It records the *input* (events, including the
/// injected `Tick`s) in submission order; the durable *log* is read from the
/// brain at save time (it is always a fold over these same events).
///
/// Recording is opt-in (`Engine::builder().record()`); a non-recording engine
/// pays nothing.
#[derive(Clone, Debug, Default)]
struct Recorder {
    events: Vec<Event>,
    /// The first injected timestamp, used as the trace's `created_at` (the
    /// `seq 0` tick — never a syscall in the core).
    created_at: Option<u64>,
}

impl Recorder {
    /// Record one event in submission order. The first `Tick` seeds `created_at`.
    fn record(&mut self, event: &Event) {
        if self.created_at.is_none() {
            if let Event::Tick { now } = event {
                self.created_at = Some(now.0);
            }
        }
        self.events.push(event.clone());
    }

    /// Pre-load the recorder with a trace's already-recorded events, so a
    /// **resumed** session (P3-4) re-saves the full history (old + new) and
    /// still replays bit-for-bit. The events are copied verbatim (including the
    /// recorded `Tick`s); `created_at` is taken from the trace's metadata so the
    /// resumed trace keeps the original session's creation time, not a new one.
    fn seed(events: Vec<Event>, created_at: Option<u64>) -> Self {
        Self { events, created_at }
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

/// Drives a [`Brain`] against real IO on tokio. Build one with
/// [`Engine::builder`].
pub struct Engine {
    brain: Brain,
    models: ModelRegistry,
    caps: CapabilityRegistry,
    policy: Arc<dyn Policy>,
    frontend: Box<dyn Frontend>,
    clock: Clock,
    tx: UnboundedSender<Event>,
    rx: UnboundedReceiver<Event>,
    tasks: HashMap<OpId, JoinHandle<()>>,
    /// Capability name per in-flight op, so tool results can be labelled when
    /// the engine observes their completion events.
    op_labels: HashMap<OpId, String>,
    /// Batches consecutive streamed text on the *render* path only, to cut
    /// per-token flush churn (ARCHITECTURE §4.4). It never touches the brain's
    /// event stream — every `ModelDelta` is still submitted — so replay stays
    /// bit-for-bit identical regardless of how the render was coalesced.
    coalescer: Coalescer,
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
            mode: SteerMode::Queue,
        });
        self.drive_to_idle().await;
    }

    /// Read-only access to the underlying brain (log, op table, …).
    pub fn brain(&self) -> &Brain {
        &self.brain
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
    /// accumulated totals (e.g. the metrics footer). Call this once after the
    /// last turn of a one-shot run, or when an interactive session exits.
    pub fn session_end(&mut self) {
        self.flush_render();
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
    /// [`baton_replay::verify`].
    pub fn trace(&self) -> Option<Trace> {
        let rec = self.recorder.as_ref()?;
        let mut trace = Trace::new(
            rec.events.clone(),
            self.brain.state().log().to_vec(),
            rec.created_at,
        );
        // Capture the policy so replay reproduces the brain's permission /
        // background branching bit-for-bit (§6.3).
        if let Some(policy) = self.policy_config.clone() {
            trace = trace.with_policy(policy);
        }
        Some(trace)
    }

    /// Save the recorded session to `path` as a trace. Errors if recording was
    /// not enabled (`TraceError::Io` with `NotFound`-style intent is avoided —
    /// returns a clear error) or the write fails.
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
                for command in commands {
                    self.perform(command).await;
                }
            }

            // No ops in flight → the turn is done. Flush any text the coalescer
            // is still holding so it lands before control returns to the caller.
            if self.brain.state().inflight_len() == 0 {
                self.flush_render();
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

    /// Drain the coalescer's buffered streamed text to the front-end as one (or
    /// zero) merged render. Called at every boundary where order matters — a
    /// lifecycle hook, a completion event, the end of a turn — so withheld text
    /// always reaches the screen before whatever follows it.
    fn flush_render(&mut self) {
        for rendered in self.coalescer.flush() {
            self.frontend.on_output(&rendered);
        }
    }

    /// Report incoming events to the front-end for observability, before the
    /// brain folds them. (Commands are reported in [`perform`](Self::perform).)
    fn observe(&mut self, event: &Event) {
        // A model/tool *completion* line must appear after the text it follows:
        // flush any buffered streamed text before rendering the lifecycle hook.
        if matches!(
            event,
            Event::ModelDone { .. } | Event::CapabilityDone { .. } | Event::CapabilityError { .. }
        ) {
            self.flush_render();
        }
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
        // Every command except `Emit` may render a front-end line (model/tool
        // start, permission, done, notice) that must follow the streamed text
        // it comes after: flush the coalescer's buffer first to keep order.
        // `Emit` itself is the coalescing path and must not self-flush.
        if !matches!(command, Command::Emit(_)) {
            self.flush_render();
        }
        match command {
            Command::StartModelCall { op, model, request } => match self.models.get(&model) {
                Some(adapter) => {
                    self.frontend.on_model_start(op, &model);
                    let tx = self.tx.clone();
                    let handle = tokio::spawn(async move {
                        let sink = ModelSink::new(op, tx.clone());
                        let event = match adapter.call(request, &sink).await {
                            Ok((output, usage)) => Event::ModelDone { op, output, usage },
                            Err(error) => Event::ModelError {
                                op,
                                error: json!({ "message": error.to_string() }),
                            },
                        };
                        let _ = tx.send(event);
                    });
                    self.tasks.insert(op, handle);
                }
                None => {
                    let _ = self.tx.send(Event::ModelError {
                        op,
                        error: json!({ "message": format!("no adapter for model {model:?}") }),
                    });
                }
            },

            Command::StartCapability { op, name, args } => match self.caps.get(&name) {
                Some(capability) => {
                    self.frontend.on_tool_start(op, &name, &args);
                    self.op_labels.insert(op, name.clone());
                    let tx = self.tx.clone();
                    let handle = tokio::spawn(async move {
                        let sink = ChunkSink::new(op, tx.clone());
                        let event = match capability.invoke(args, &sink).await {
                            Ok(result) => Event::CapabilityDone {
                                op,
                                result,
                                version: None,
                            },
                            Err(error) => Event::CapabilityError {
                                op,
                                error,
                                conflict: None,
                            },
                        };
                        let _ = tx.send(event);
                    });
                    self.tasks.insert(op, handle);
                }
                None => {
                    let _ = self.tx.send(Event::CapabilityError {
                        op,
                        error: json!({ "error": format!("unknown capability: {name}") }),
                        conflict: None,
                    });
                }
            },

            Command::RequestPermission { op, request } => {
                let decision = self.policy.decide(&request).await;
                self.frontend.on_permission(&request.capability, &decision);
                let _ = self.tx.send(Event::PermissionDecision { op, decision });
            }

            Command::AskUser { op, prompt } => {
                let answer = ask_user(&prompt.message).await;
                let _ = self.tx.send(Event::UserAnswer {
                    op,
                    answer: Value::String(answer),
                });
            }

            Command::Cancel { op } => {
                if let Some(handle) = self.tasks.remove(&op) {
                    handle.abort();
                }
                let _ = self.tx.send(Event::OpCancelled { op });
            }

            // Cosmetic output goes through the coalescer: consecutive streamed
            // text is batched into fewer, larger renders (ARCHITECTURE §4.4).
            // The brain already saw every delta (the engine submits them all),
            // so this affects only what the front-end draws, never the log.
            Command::Emit(event) => {
                for rendered in self.coalescer.push(event) {
                    self.frontend.on_output(&rendered);
                }
            }

            // The recorder captures the event stream at `submit` (so the trace
            // is always buildable on demand via `Engine::trace`); a checkpoint
            // just drops finished task handles so they don't accumulate. A host
            // that wanted incremental on-disk persistence could `save_trace`
            // here too.
            Command::Checkpoint => self.tasks.retain(|_, h| !h.is_finished()),

            Command::Done { reason } => self.frontend.on_done(&reason),

            // Forward-compatible: a newer core may add commands this host
            // doesn't know about yet (ARCHITECTURE §2.4).
            other => self
                .frontend
                .on_notice(&format!("(unhandled command: {other:?})")),
        }
    }
}

/// Prompt the user for a free-form answer (off the async runtime threads).
async fn ask_user(message: &str) -> String {
    let message = message.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        print!("{message} ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        line.trim().to_string()
    })
    .await
    .unwrap_or_default()
}

fn system_clock() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Builds an [`Engine`]: register models + capabilities, then `build()`. The
/// builder also assembles the brain's [`StaticPolicy`] from the registered
/// capabilities (their schemas become the advertised tools, and the ones that
/// require permission become the gated set), so the caller doesn't repeat that.
pub struct EngineBuilder {
    models: ModelRegistry,
    caps: CapabilityRegistry,
    policy: Option<Arc<dyn Policy>>,
    frontend: Option<Box<dyn Frontend>>,
    clock: Option<Clock>,
    selector: ModelSelector,
    system_prompt: Option<String>,
    sampling: SamplingParams,
    record: bool,
    /// When set, the brain is pre-seeded by replaying this trace's recorded
    /// events into it (with zero IO), and the recorder is pre-loaded with those
    /// same events so the continued session re-saves the full history (P3-4).
    resume: Option<Trace>,
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self {
            models: ModelRegistry::new(),
            caps: CapabilityRegistry::new(),
            policy: None,
            frontend: None,
            clock: None,
            selector: ModelSelector::named("big"),
            system_prompt: None,
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
        if self.models.get(&selector).is_none() && self.selector_is_default() {
            self.selector = selector.clone();
        }
        self.models.register(selector, adapter);
        self
    }

    fn selector_is_default(&self) -> bool {
        self.selector == ModelSelector::named("big")
    }

    /// Override which logical selector the turn policy calls each turn.
    pub fn default_model(mut self, selector: ModelSelector) -> Self {
        self.selector = selector;
        self
    }

    /// Register a capability (tool).
    pub fn capability(mut self, capability: Arc<dyn crate::Capability>) -> Self {
        self.caps.register(capability);
        self
    }

    /// Set the permission policy (default: [`AllowAll`]).
    pub fn policy(mut self, policy: Arc<dyn Policy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Set the front-end (default: [`StdoutFrontend`]).
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

    /// Resume a session from a saved [`Trace`] (P3-4). The built engine's brain
    /// is reconstructed by re-feeding the trace's recorded events into it (with
    /// **zero IO** — the host does *not* re-run the model/shell/http for events
    /// that already happened; it only re-folds them to rebuild `BrainState`,
    /// exactly as [`baton_replay::replay`] does). New live turns then continue
    /// from that state.
    ///
    /// Resuming implies recording (so the continued session can be re-saved as a
    /// trace that still verifies bit-for-bit): the recorder is pre-loaded with
    /// the trace's events, and any new events are appended after them. The
    /// session's [`TurnPolicy`] is restored from the trace ([`policy_from_trace`])
    /// so the continued session branches identically; a trace without a captured
    /// policy falls back to the default.
    pub fn resume(mut self, trace: Trace) -> Self {
        self.record = true;
        self.resume = Some(trace);
        self
    }

    pub fn build(self) -> Engine {
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
                // the recorded events into the fresh brain and discard the commands
                // they re-emit (the host must not re-run the model/shell/http for
                // work that already happened — this only rebuilds `BrainState`,
                // exactly like `baton_replay::replay`, via the shared `drive` fold).
                let _ = baton_replay::drive(&mut brain, &events);
                // Pre-seed the recorder with the same events (moved, not cloned) so
                // a later `save_trace` carries old + new (ARCHITECTURE §6.3).
                let recorder = Recorder::seed(events, trace.meta.created_at);
                (brain, Some(recorder), trace.policy)
            }
            None => {
                let mut policy = StaticPolicy::default()
                    .with_model(self.selector.clone())
                    .with_tools(self.caps.schemas())
                    .with_permissioned(self.caps.permissioned_names())
                    .with_background(self.caps.background_names())
                    .with_params(self.sampling);
                if let Some(system) = self.system_prompt {
                    policy = policy.with_system_prompt(system);
                }
                // Serialize the policy once (for recorded traces) before it moves
                // into the brain. `StaticPolicy` is serde-able; best-effort.
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
            policy: self.policy.unwrap_or_else(|| Arc::new(AllowAll)),
            frontend: self
                .frontend
                .unwrap_or_else(|| Box::new(StdoutFrontend::new())),
            clock: self.clock.unwrap_or_else(|| Arc::new(system_clock)),
            tx,
            rx,
            tasks: HashMap::new(),
            op_labels: HashMap::new(),
            coalescer: Coalescer::new(),
            recorder,
            policy_config,
        }
    }
}
