//! The tokio driver loop (ARCHITECTURE §2.3) and its builder.
//!
//! The driver is the *entire* integration surface: drain `brain.poll()`,
//! perform each command (spawning one task per in-flight op), await the next
//! event from any source, `brain.submit()` it, repeat. All concurrency lives
//! here; the brain stays synchronous and single-threaded.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use hugr_core::{
    AgentSeed, Brain, Command, ContextPlan, Event, HookPhase, ModelSelector, OpId, RoutingPolicy,
    SamplingParams, SkillDescriptor, StaticPolicy, SteerMode, Timestamp, TodoItem, ToolSchema,
    Value,
};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use hugr_replay::{Trace, TraceError, policy_from_trace};

use crate::ChunkSink;
use crate::capability::CapabilityRegistry;
use crate::coalesce::Coalescer;
use crate::frontend::{Frontend, StdoutFrontend};
use crate::model::{ModelRegistry, ModelSink};
use crate::policy::{AllowAll, Policy};

/// How aggressively a recording engine persists its trace checkpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CheckpointCadence {
    /// Only save when the brain emits [`Command::Checkpoint`].
    OnCommand,
    /// Save after every host event submitted to the brain. This is the durable
    /// crash-resume mode: a kill during a model/tool op still leaves a trace
    /// whose fold reconstructs the in-flight op table (ARCHITECTURE §15.1).
    EveryEvent,
    /// Save after every N host events, plus on [`Command::Checkpoint`].
    EveryNEvents(usize),
}

impl CheckpointCadence {
    fn due_after_event(self, events_since_checkpoint: usize) -> bool {
        match self {
            CheckpointCadence::OnCommand => false,
            CheckpointCadence::EveryEvent => true,
            CheckpointCadence::EveryNEvents(n) => events_since_checkpoint >= n.max(1),
        }
    }
}

/// State shared between the driver loop and its background checkpoint writers
/// (see [`Engine::checkpoint`]). Writes are **single-flight**: `writing` gates
/// scheduling (a checkpoint that comes due mid-write marks the engine dirty
/// instead of stacking a second writer), and `written` — the highest snapshot
/// generation persisted — guards the file itself, so a stale writer can never
/// clobber a newer snapshot. Latest state always wins.
struct CheckpointShared {
    /// Highest snapshot generation persisted so far. Holding this mutex also
    /// serializes the actual file writes: two writers never run concurrently
    /// against the same path.
    written: Mutex<u64>,
    /// True while a background write is in flight (the single-flight gate).
    writing: AtomicBool,
    /// Errors from background writes, drained to the front-end on the loop
    /// (the driver methods are intentionally infallible).
    errors: Mutex<Vec<String>>,
}

impl CheckpointShared {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            written: Mutex::new(0),
            writing: AtomicBool::new(false),
            errors: Mutex::new(Vec::new()),
        })
    }
}

/// Write one checkpoint snapshot under the shared file lock. A writer whose
/// snapshot generation is older than what is already on disk skips the write
/// entirely (a newer snapshot supersedes it — latest state wins).
fn write_checkpoint(
    shared: &CheckpointShared,
    generation: u64,
    trace: &Trace,
    compaction: TraceCompaction,
    path: &std::path::Path,
) -> Result<(), TraceError> {
    let mut written = shared.written.lock().unwrap();
    if generation <= *written {
        return Ok(());
    }
    match compaction {
        TraceCompaction::PreserveFull => trace.save_atomic(path)?,
    }
    *written = generation;
    Ok(())
}

/// How a resumed host reconciles ops that were in flight when the previous
/// process died.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrashResumePolicy {
    /// Append recorded [`Event::OpCancelled`] events for stale in-flight ops and
    /// let the brain fold the cancellation exactly as if the old host had
    /// confirmed an abort before exiting. This is replay-safe and conservative;
    /// idempotent re-issue can be added as another host policy later.
    CancelInflight,
}

/// Durable log compaction policy for native checkpoints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TraceCompaction {
    /// Preserve the full event stream and consolidated log. Phase 7 makes the
    /// policy explicit, but keeps the default lossless because the log is the
    /// source of truth and destructive compaction would break replay.
    PreserveFull,
}

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
struct Recorder {
    events: Vec<Event>,
    /// The brain→host commands, in the order the driver drained them from
    /// `brain.poll()` (the replay *output* verified against replay, §6.3).
    commands: Vec<Command>,
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

    /// Record commands in the order the driver drained them from the brain, so
    /// the trace's command sequence matches what replay re-emits (§6.3).
    fn record_commands(&mut self, commands: &[Command]) {
        self.commands.extend_from_slice(commands);
    }

    /// Pre-load the recorder with a trace's already-recorded events **and** the
    /// commands re-derived by re-folding them, so a **resumed** session (P3-4)
    /// re-saves the full history (old + new) and still verifies bit-for-bit. The
    /// events are copied verbatim (including the recorded `Tick`s); `commands`
    /// come from the resume re-fold (see [`hugr_replay::drive`]) rather than the
    /// old trace's `commands`, so a resumed *old* trace (empty commands) still
    /// gets a complete, self-consistent command sequence. `created_at` is taken
    /// from the trace's metadata so the resumed trace keeps the original
    /// session's creation time, not a new one.
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
    /// Optional durable checkpoint target. When set, the engine writes the
    /// current trace atomically according to `checkpoint_cadence`.
    checkpoint_path: Option<PathBuf>,
    checkpoint_cadence: CheckpointCadence,
    events_since_checkpoint: usize,
    /// Set when a checkpoint came due while a background write was still in
    /// flight: the next opportunity (or the final flush) writes the latest
    /// state instead of queueing a second concurrent writer.
    checkpoint_dirty: bool,
    /// Monotone snapshot generation for checkpoint writes (latest wins).
    checkpoint_gen: u64,
    checkpoint_shared: Arc<CheckpointShared>,
    /// Wakes the driver loop when a background checkpoint write finishes, so a
    /// dirty engine re-checkpoints promptly even if no further event arrives
    /// (e.g. mid-model-call, exactly when crash durability matters).
    ckpt_done_tx: UnboundedSender<()>,
    ckpt_done_rx: UnboundedReceiver<()>,
    compaction: TraceCompaction,
    /// The brain's policy config, serialized once at build time, so a recorded
    /// trace can carry it (the brain branches on the policy's pure decisions —
    /// permission/background — so replay needs the same policy, §6.3).
    policy_config: Option<serde_json::Value>,
    /// The logical model a sub-agent uses when its config doesn't name one
    /// (ARCHITECTURE §13.1); the child reuses the host's model registry.
    default_model: ModelSelector,
    session_started: bool,
}

impl Engine {
    pub fn builder() -> EngineBuilder {
        EngineBuilder::new()
    }

    /// Submit one conversational user message and drive the resulting turn (and
    /// any tool round-trips) to completion.
    pub async fn user_turn(&mut self, text: String) {
        self.ensure_session_started();
        self.submit(Event::UserInput {
            content: json!(text),
            mode: SteerMode::Queue,
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

    /// Fire one manual compaction pass and drive it to completion. If there is
    /// no compactable span, the brain emits a notice and remains idle.
    pub async fn compact_context(&mut self) {
        self.fire_hook(
            HookPhase::Compaction,
            "builtin_compaction",
            json!({ "message": "manual compaction requested" }),
        );
        self.submit(Event::CompactContext);
        self.drive_to_idle().await;
    }

    /// Force the next normal model turn to use `selector`, or clear a pending
    /// override with `None`. This is recorded as a host event so replay stays
    /// deterministic.
    pub fn override_next_model(&mut self, selector: Option<ModelSelector>) {
        self.submit(Event::ModelOverride { selector });
    }

    /// Persist a user-accepted/edited plan as durable future context
    /// (ROADMAP_2 D4).
    pub fn accept_plan(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.submit(Event::PlanAccepted {
            est_tokens: estimate_text_tokens(&text),
            text,
        });
    }

    /// Persist the current todo/task progress snapshot (ROADMAP_2 D5).
    pub fn update_todos(&mut self, items: Vec<TodoItem>) {
        let rendered = render_todos_for_estimate(&items);
        self.submit(Event::TodoUpdated {
            items,
            est_tokens: estimate_text_tokens(&rendered),
        });
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
    /// last turn of a one-shot run, or when an interactive session exits. Also
    /// flushes a final synchronous checkpoint so shutdown never loses data to a
    /// still-in-flight background write.
    pub fn session_end(&mut self) {
        self.flush_render();
        self.flush_checkpoint_sync();
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
        self.events_since_checkpoint += 1;
        if self
            .checkpoint_cadence
            .due_after_event(self.events_since_checkpoint)
        {
            self.checkpoint();
        }
    }

    fn ensure_session_started(&mut self) {
        if self.session_started {
            return;
        }
        self.session_started = true;
        self.fire_hook(
            HookPhase::SessionStart,
            "builtin_session_start",
            json!({ "message": "session started" }),
        );
    }

    fn fire_hook(&mut self, phase: HookPhase, name: &str, result: Value) {
        let est_tokens = estimate_value_tokens(&result);
        self.submit(Event::HookFired {
            phase,
            name: name.to_string(),
            result,
            est_tokens,
        });
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

    /// Schedule a checkpoint of the current recorded trace to the configured
    /// target, if any. The serialization + atomic write-then-rename runs on a
    /// blocking task **off the driver loop**; writes are single-flight — if one
    /// is still in progress the engine marks itself dirty and writes the latest
    /// state at the next opportunity (or at the final flush) instead of stacking
    /// a second concurrent writer for the same path. A checkpoint with nothing
    /// new since the last snapshot is skipped entirely. Errors are reported to
    /// the front-end as notices because the driver methods are infallible.
    fn checkpoint(&mut self) {
        if self.checkpoint_path.is_none() {
            return;
        }
        self.drain_checkpoint_errors();
        // Nothing changed since the last scheduled snapshot: skip entirely.
        if self.events_since_checkpoint == 0 && !self.checkpoint_dirty {
            return;
        }
        // Single-flight: a write is still in progress. Mark dirty; the latest
        // state is written when the next checkpoint opportunity (or the final
        // flush) finds the writer free.
        if self.checkpoint_shared.writing.load(Ordering::Acquire) {
            self.checkpoint_dirty = true;
            return;
        }
        let Some((trace, path, generation)) = self.checkpoint_snapshot() else {
            return;
        };
        let compaction = self.compaction;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let shared = self.checkpoint_shared.clone();
                let done = self.ckpt_done_tx.clone();
                shared.writing.store(true, Ordering::Release);
                handle.spawn_blocking(move || {
                    if let Err(err) =
                        write_checkpoint(&shared, generation, &trace, compaction, &path)
                    {
                        shared
                            .errors
                            .lock()
                            .unwrap()
                            .push(format!("checkpoint failed ({}): {err}", path.display()));
                    }
                    shared.writing.store(false, Ordering::Release);
                    // Wake the driver loop: if it went dirty while this write
                    // was in flight, it re-checkpoints with the latest state.
                    let _ = done.send(());
                });
            }
            // No runtime on this thread (e.g. events submitted before the loop
            // starts): fall back to the old synchronous write.
            Err(_) => {
                if let Err(err) = write_checkpoint(
                    &self.checkpoint_shared,
                    generation,
                    &trace,
                    compaction,
                    &path,
                ) {
                    self.frontend
                        .on_notice(&format!("checkpoint failed ({}): {err}", path.display()));
                }
            }
        }
    }

    /// Snapshot the current trace and claim the next write generation, resetting
    /// the "something changed" tracking. Returns `None` (with a notice) if the
    /// engine is not recording.
    fn checkpoint_snapshot(&mut self) -> Option<(Trace, PathBuf, u64)> {
        let path = self.checkpoint_path.clone()?;
        let Some(trace) = self.trace() else {
            self.frontend.on_notice(&format!(
                "checkpoint failed ({}): engine is not recording; checkpoint requires .record()",
                path.display()
            ));
            return None;
        };
        self.checkpoint_gen += 1;
        self.events_since_checkpoint = 0;
        self.checkpoint_dirty = false;
        Some((trace, path, self.checkpoint_gen))
    }

    /// At a turn boundary, catch up a checkpoint that went dirty while a write
    /// was in flight: wait for the writer to finish (the wakeup channel), then
    /// schedule one more write with the latest state. Keeps the on-disk trace
    /// current while the engine idles between turns.
    async fn settle_checkpoint(&mut self) {
        while self.checkpoint_dirty {
            if self.checkpoint_shared.writing.load(Ordering::Acquire)
                && self.ckpt_done_rx.recv().await.is_none()
            {
                return; // unreachable: the engine holds a sender
            }
            self.checkpoint();
        }
        self.drain_checkpoint_errors();
    }

    /// Report any errors from background checkpoint writes to the front-end.
    fn drain_checkpoint_errors(&mut self) {
        let errors: Vec<String> =
            std::mem::take(&mut *self.checkpoint_shared.errors.lock().unwrap());
        for err in errors {
            self.frontend.on_notice(&err);
        }
    }

    /// Synchronously flush a final checkpoint at shutdown/drop, so the tail of
    /// the session is never lost to an unfinished background write. Waits for
    /// any in-flight writer via the shared file lock; a still-pending stale
    /// writer that runs later skips itself (its generation is superseded).
    fn flush_checkpoint_sync(&mut self) {
        if self.checkpoint_path.is_none() {
            return;
        }
        self.drain_checkpoint_errors();
        // Skip only when nothing changed AND no write is in flight — an
        // in-flight write may still be queued behind us at runtime shutdown, so
        // rewriting the latest state here is the safe way to guarantee it lands.
        if self.events_since_checkpoint == 0
            && !self.checkpoint_dirty
            && !self.checkpoint_shared.writing.load(Ordering::Acquire)
        {
            return;
        }
        let Some((trace, path, generation)) = self.checkpoint_snapshot() else {
            return;
        };
        let compaction = self.compaction;
        if let Err(err) = write_checkpoint(
            &self.checkpoint_shared,
            generation,
            &trace,
            compaction,
            &path,
        ) {
            self.frontend
                .on_notice(&format!("checkpoint failed ({}): {err}", path.display()));
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
                // — the property `hugr_replay::verify` now checks bit-for-bit
                // (command ordering never reaches the log, §6.3).
                if let Some(rec) = self.recorder.as_mut() {
                    rec.record_commands(&commands);
                }
                for command in commands {
                    self.perform(command).await;
                }
            }

            // No ops in flight → the turn is done. Flush any text the coalescer
            // is still holding so it lands before control returns to the caller,
            // and settle any checkpoint that went dirty during the turn so the
            // on-disk trace isn't stale while the engine sits idle.
            if self.brain.state().inflight_len() == 0 {
                self.flush_render();
                self.settle_checkpoint().await;
                break;
            }

            // Otherwise block until any task produces the next event — or a
            // finished background checkpoint write asks for a dirty rewrite.
            tokio::select! {
                maybe_event = self.rx.recv() => match maybe_event {
                    Some(event) => {
                        // Every tool-shaped completion (capability *or*
                        // sub-agent — the brain folds both tool-result-shaped)
                        // fires the PostTool hook, off the same classification
                        // `observe` uses so the two can never diverge.
                        let post_tool_hook =
                            tool_shaped_completion(&event).map(|(op, payload, is_error)| {
                                if is_error {
                                    json!({ "op": op.0, "ok": false, "error": payload })
                                } else {
                                    json!({ "op": op.0, "ok": true, "result": payload })
                                }
                            });
                        self.observe(&event);
                        self.submit(event);
                        if let Some(result) = post_tool_hook {
                            self.fire_hook(HookPhase::PostTool, "builtin_post_tool", result);
                        }
                    }
                    None => break,
                },
                _ = self.ckpt_done_rx.recv() => {
                    // A background checkpoint write finished. If events arrived
                    // while it was in flight (dirty), write again now with the
                    // latest state — single-flight means "write again when it
                    // finishes", never a second concurrent writer. This keeps
                    // the on-disk trace fresh even when the turn is blocked on
                    // a long model/tool op (the crash-durability case).
                    if self.checkpoint_dirty {
                        self.checkpoint();
                    } else {
                        self.drain_checkpoint_errors();
                    }
                }
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
        // Tool-shaped completions come from the shared classification (see
        // [`tool_shaped_completion`]) so this render and the PostTool hook in
        // `drive_to_idle` can never diverge.
        if let Event::ModelDone { op, usage, .. } = event {
            self.flush_render();
            self.frontend.on_model_end(*op, usage);
            return;
        }
        if let Some((op, payload, is_error)) = tool_shaped_completion(event) {
            self.flush_render();
            let name = self.op_labels.remove(&op).unwrap_or_default();
            self.frontend.on_tool_end(op, &name, payload, is_error);
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
                    self.fire_hook(
                        HookPhase::PreTool,
                        "builtin_pre_tool",
                        json!({ "op": op.0, "capability": name.clone(), "args": args.clone() }),
                    );
                    self.frontend.on_tool_start(op, &name, &args);
                    self.op_labels.insert(op, name.clone());
                    let tx = self.tx.clone();
                    let handle = tokio::spawn(async move {
                        let sink = ChunkSink::new(op, tx.clone());
                        let event = match capability.invoke(args, &sink).await {
                            Ok(result) => {
                                let version = capability.result_version(&result);
                                Event::CapabilityDone {
                                    op,
                                    est_tokens: estimate_value_tokens(&result),
                                    result,
                                    version,
                                }
                            }
                            Err(error) => {
                                let conflict = capability.conflict_version(&error);
                                Event::CapabilityError {
                                    op,
                                    est_tokens: estimate_value_tokens(&error),
                                    error,
                                    conflict,
                                }
                            }
                        };
                        let _ = tx.send(event);
                    });
                    self.tasks.insert(op, handle);
                }
                None => {
                    let _ = self.tx.send(Event::CapabilityError {
                        op,
                        est_tokens: estimate_value_tokens(&json!({
                            "error": format!("unknown capability: {name}")
                        })),
                        error: json!({ "error": format!("unknown capability: {name}") }),
                        conflict: None,
                    });
                }
            },

            // A sub-agent is another brain the host drives on its own task
            // (ARCHITECTURE §13). It reuses (a subset of) our model + capability
            // registries; its progress streams back as events keyed by `op` and
            // its digest returns as `AgentDone`. Tracked in `tasks` so a `Cancel`
            // aborts the whole subtree.
            Command::StartAgent { op, config, seed } => {
                // A sub-agent spawn is tool-shaped: fire the same PreTool hook
                // a capability start does (its completion fires PostTool via
                // `tool_shaped_completion`). Skill activations, by contrast,
                // happen entirely inside the brain with no host event, so no
                // Pre/PostTool hook can fire for them — a known limitation.
                let label = agent_label(&config);
                self.fire_hook(
                    HookPhase::PreTool,
                    "builtin_pre_tool",
                    json!({ "op": op.0, "capability": label.clone(), "args": config.clone() }),
                );
                self.frontend.on_tool_start(op, &label, &config);
                self.op_labels.insert(op, label);
                let handle = tokio::spawn(crate::agent::run_agent(
                    op,
                    config,
                    seed,
                    self.models.clone(),
                    self.caps.clone(),
                    self.policy.clone(),
                    self.default_model.clone(),
                    self.clock.clone(),
                    self.tx.clone(),
                ));
                self.tasks.insert(op, handle);
            }

            Command::RequestPermission { op, request } => {
                let decision = self.policy.decide(&request).await;
                self.frontend.on_permission(&request.capability, &decision);
                let est_tokens = permission_decision_est_tokens(&decision);
                let _ = self.tx.send(Event::PermissionDecision {
                    op,
                    decision,
                    est_tokens,
                });
            }

            Command::AskUser { op, prompt } => {
                let answer = ask_user(&prompt.message).await;
                let est_tokens = estimate_text_tokens(&answer);
                let _ = self.tx.send(Event::UserAnswer {
                    op,
                    answer: Value::String(answer),
                    est_tokens,
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
            Command::Checkpoint => {
                self.tasks.retain(|_, h| !h.is_finished());
                self.checkpoint();
            }

            Command::Done { reason } => {
                self.fire_hook(
                    HookPhase::Stop,
                    "builtin_stop",
                    json!({ "reason": format!("{reason:?}") }),
                );
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

impl Drop for Engine {
    fn drop(&mut self) {
        // A dropped engine still flushes its final checkpoint synchronously, so
        // no submitted event is lost even if the caller never reached
        // `session_end` (e.g. an early return). A no-op when nothing changed
        // since the last completed write, or when checkpointing is off.
        self.flush_checkpoint_sync();
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

/// Rough token estimate for a piece of text (~4 bytes per token, min 1).
/// Public so embedders (e.g. the CLI) can size opaque payloads consistently
/// with the host's own estimates.
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

pub(crate) fn permission_decision_est_tokens(decision: &hugr_core::Decision) -> u32 {
    match decision {
        hugr_core::Decision::Allow => 0,
        hugr_core::Decision::Deny { reason } => estimate_text_tokens(reason),
        _ => 0,
    }
}

fn render_todos_for_estimate(items: &[TodoItem]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            format!(
                "{}. [{}] {}",
                idx + 1,
                if item.done { "x" } else { " " },
                item.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
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
    /// The selector explicitly chosen via [`default_model`](Self::default_model),
    /// if any. Tracked separately from `first_model` so an explicit choice that
    /// happens to equal the built-in fallback (e.g. `named("medium")`) is still
    /// honored and can never be stolen by a later registration.
    default_model: Option<ModelSelector>,
    /// The first selector registered via [`model`](Self::model) — the documented
    /// convenience fallback when no explicit default was set.
    first_model: Option<ModelSelector>,
    system_prompt: Option<String>,
    sampling: SamplingParams,
    /// Capabilities that spawn sub-agents (ARCHITECTURE §13): each advertises a
    /// tool schema to the model and carries a fork seed strategy (§14).
    agents: Vec<(ToolSchema, AgentSeed)>,
    skills: Vec<SkillDescriptor>,
    record: bool,
    checkpoint_path: Option<PathBuf>,
    checkpoint_cadence: CheckpointCadence,
    crash_resume: CrashResumePolicy,
    compaction: TraceCompaction,
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
            default_model: None,
            first_model: None,
            system_prompt: None,
            sampling: SamplingParams::default(),
            agents: Vec::new(),
            skills: Vec::new(),
            record: false,
            checkpoint_path: None,
            checkpoint_cadence: CheckpointCadence::OnCommand,
            crash_resume: CrashResumePolicy::CancelInflight,
            compaction: TraceCompaction::PreserveFull,
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

    /// Register a **sub-agent** tool (ARCHITECTURE §13): the model sees `schema`
    /// as an ordinary tool, but invoking it spawns a child brain (seeded per
    /// `seed`, §14) which the host runs on its own task and whose digest returns
    /// as the tool's result. The child reuses this host's model + capability
    /// registries (optionally narrowed by a `tools` allowlist in its args).
    pub fn agent(mut self, schema: ToolSchema, seed: AgentSeed) -> Self {
        self.agents.push((schema, seed));
        self
    }

    /// Register skill descriptors. The brain advertises them as lightweight
    /// model-invocable tools and records activation durably (ROADMAP_2 C5/C6).
    pub fn skills(mut self, skills: impl IntoIterator<Item = SkillDescriptor>) -> Self {
        self.skills.extend(skills);
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

    /// Persist this recording to `path` during the run. This implies
    /// [`record`](Self::record): checkpoints are just the current trace written
    /// atomically at the chosen cadence (ARCHITECTURE §12.2/§15.1).
    pub fn checkpoint(mut self, path: impl Into<PathBuf>, cadence: CheckpointCadence) -> Self {
        self.record = true;
        self.checkpoint_path = Some(path.into());
        self.checkpoint_cadence = cadence;
        self
    }

    /// Set how a resumed trace reconciles stale in-flight ops left by a crash.
    pub fn crash_resume_policy(mut self, policy: CrashResumePolicy) -> Self {
        self.crash_resume = policy;
        self
    }

    /// Set the checkpoint compaction policy. The Phase 7 native host exposes
    /// the policy explicitly and defaults to lossless full-trace preservation.
    pub fn compaction(mut self, compaction: TraceCompaction) -> Self {
        self.compaction = compaction;
        self
    }

    /// Resume a session from a saved [`Trace`] (P3-4). The built engine's brain
    /// is reconstructed by re-feeding the trace's recorded events into it (with
    /// **zero IO** — the host does *not* re-run the model/shell/http for events
    /// that already happened; it only re-folds them to rebuild `BrainState`,
    /// exactly as [`hugr_replay::replay`] does). New live turns then continue
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
                // the recorded events into the fresh brain (the host must not
                // re-run the model/shell/http for work that already happened —
                // this only rebuilds `BrainState`, exactly like
                // `hugr_replay::replay`, via the shared `drive` fold). We *keep*
                // the re-emitted commands (rather than discard them) to seed the
                // recorder's command sequence: re-deriving them here makes the
                // resumed trace's `commands` self-consistent with its events even
                // when the original trace predates command recording (empty
                // `commands`), so the re-saved trace still verifies bit-for-bit.
                let resume_commands = hugr_replay::drive(&mut brain, &events);
                // Pre-seed the recorder with the same events (moved, not cloned)
                // and the re-derived commands so a later `save_trace` carries
                // old + new (ARCHITECTURE §6.3).
                let mut recorder = Recorder::seed(events, resume_commands, trace.meta.created_at);
                reconcile_crashed_ops(&mut brain, &mut recorder, self.crash_resume, &clock);
                (brain, Some(recorder), trace.policy)
            }
            None => {
                // Advertise both capability tools and sub-agent tools to the
                // model; the brain routes agent-named calls to `StartAgent`.
                let mut tools = self.caps.schemas();
                tools.extend(self.agents.iter().map(|(schema, _)| schema.clone()));
                let mut base_policy = StaticPolicy::default()
                    .with_model(default_model.clone())
                    .with_tools(tools)
                    .with_permissioned(self.caps.permissioned_names())
                    .with_background(self.caps.background_names())
                    .with_skills(self.skills)
                    .with_params(self.sampling);
                for (schema, seed) in &self.agents {
                    base_policy = base_policy.with_agent(schema.name.clone(), *seed);
                }
                if let Some(system) = self.system_prompt {
                    base_policy = base_policy.with_system_prompt(system);
                }
                let policy = RoutingPolicy::new(base_policy);
                // Serialize the policy once (for recorded traces) before it moves
                // into the brain. `RoutingPolicy` is serde-able; best-effort.
                let policy_config = self
                    .record
                    .then(|| serde_json::to_value(&policy).ok())
                    .flatten();
                let brain = Brain::new(Box::new(policy));
                (brain, self.record.then(Recorder::default), policy_config)
            }
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let (ckpt_done_tx, ckpt_done_rx) = mpsc::unbounded_channel();
        Engine {
            brain,
            models: self.models,
            caps: self.caps,
            policy: self.policy.unwrap_or_else(|| Arc::new(AllowAll)),
            frontend: self
                .frontend
                .unwrap_or_else(|| Box::new(StdoutFrontend::new())),
            clock,
            tx,
            rx,
            tasks: HashMap::new(),
            op_labels: HashMap::new(),
            coalescer: Coalescer::new(),
            recorder,
            checkpoint_path: self.checkpoint_path,
            checkpoint_cadence: self.checkpoint_cadence,
            events_since_checkpoint: 0,
            checkpoint_dirty: false,
            checkpoint_gen: 0,
            checkpoint_shared: CheckpointShared::new(),
            ckpt_done_tx,
            ckpt_done_rx,
            compaction: self.compaction,
            policy_config,
            default_model,
            session_started: false,
        }
    }
}

fn reconcile_crashed_ops(
    brain: &mut Brain,
    recorder: &mut Recorder,
    policy: CrashResumePolicy,
    clock: &Clock,
) {
    match policy {
        CrashResumePolicy::CancelInflight => {
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
            // `StartModelCall` if an interrupt was pending when the process
            // died. The pre-crash turn is over: a resumed engine must start
            // quiescent, so drain and discard those commands here. Otherwise
            // they would fire at the start of the next live turn — a spurious
            // stop hook, or a stale pre-crash model call racing the new one.
            // They ARE still recorded: replaying the trace re-emits them, so
            // the recorded command sequence must include them for `verify` to
            // match bit-for-bit — "drained, not performed" is a host choice
            // invisible to the pure fold.
            recorder.record_commands(&brain.poll());
        }
    }
}

/// Classify an event as a **tool-shaped completion**: `(op, payload, is_error)`
/// for every event the brain folds tool-result-shaped — a capability finishing
/// *or* a sub-agent returning its digest (ARCHITECTURE §13). This is the single
/// place that lists them, shared by `Engine::observe` (the front-end tool-end
/// render) and the PostTool hook in `drive_to_idle`, so the two can never
/// diverge. Skill activations are *not* here: they happen entirely inside the
/// brain (no host event crosses this boundary), so no Pre/PostTool hook fires
/// for them — a known limitation.
fn tool_shaped_completion(event: &Event) -> Option<(OpId, &Value, bool)> {
    match event {
        Event::CapabilityDone { op, result, .. } | Event::AgentDone { op, result, .. } => {
            Some((*op, result, false))
        }
        Event::CapabilityError { op, error, .. } | Event::AgentError { op, error, .. } => {
            Some((*op, error, true))
        }
        _ => None,
    }
}

/// A display label for a sub-agent op — its config's `name` if given, else the
/// prompt's opening words, else just `"agent"`. Cosmetic (front-end only).
fn agent_label(config: &Value) -> String {
    if let Some(name) = config.get("name").and_then(|v| v.as_str()) {
        return format!("agent:{name}");
    }
    match config.get("prompt").and_then(|v| v.as_str()) {
        Some(prompt) => {
            let head: String = prompt
                .split_whitespace()
                .take(4)
                .collect::<Vec<_>>()
                .join(" ");
            format!("agent:{head}")
        }
        None => "agent".to_string(),
    }
}
