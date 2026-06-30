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

use crate::ChunkSink;
use crate::capability::CapabilityRegistry;
use crate::frontend::{Frontend, StdoutFrontend};
use crate::model::{ModelRegistry, ModelSink};
use crate::policy::{AllowAll, Policy};

/// A source of (host-side) wall-clock time, injected into the brain as `Tick`
/// events so the brain itself never reads a clock (ARCHITECTURE §6.1).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

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

    /// Signal the front-end that the session is finishing, so it can render any
    /// accumulated totals (e.g. the metrics footer). Call this once after the
    /// last turn of a one-shot run, or when an interactive session exits.
    pub fn session_end(&mut self) {
        self.frontend.on_session_end();
    }

    /// Feed an event in, stamping it with a fresh injected `Tick` first.
    fn submit(&mut self, event: Event) {
        let now = Timestamp((self.clock)());
        self.brain.submit(Event::Tick { now });
        self.brain.submit(event);
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

            Command::Emit(event) => self.frontend.on_output(&event),

            // Phase 3 persists the trace here; Phase 1 just drops finished
            // task handles so they don't accumulate.
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

    pub fn build(self) -> Engine {
        let mut policy_builder = StaticPolicy::default()
            .with_model(self.selector.clone())
            .with_tools(self.caps.schemas())
            .with_permissioned(self.caps.permissioned_names())
            .with_params(self.sampling);
        if let Some(system) = self.system_prompt {
            policy_builder = policy_builder.with_system_prompt(system);
        }

        let (tx, rx) = mpsc::unbounded_channel();
        Engine {
            brain: Brain::new(Box::new(policy_builder)),
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
        }
    }
}
