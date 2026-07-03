//! The sub-agent runner (ARCHITECTURE §13).
//!
//! A sub-agent is **not a special subsystem** — it is *another
//! [`hugr_core::Brain`]* the host drives to completion on its own task, exactly
//! like the top-level [`Engine`](crate::Engine) drives the root brain. Because
//! the core is tiny, pure and runtime-free, spawning one is cheap, and an
//! arbitrarily deep tree of agents is just a tree of brains (bounded by the
//! host's [`max_agent_depth`](crate::EngineBuilder::max_agent_depth)).
//!
//! This module implements the **in-process** isolation mode (§13.2): the child
//! runs on a spawned tokio task, reusing (a subset of) the parent's model and
//! capability registries. Its progress streams back to the parent as ordinary
//! [`Event`]s keyed by the parent's op, and its final digest returns as
//! [`Event::AgentDone`] (§13.1) — the parent folds it like any other op result.
//!
//! The runner is **generic**: which model tier and tool allowlist an agent kind
//! defaults to is registration data ([`AgentDefaults`], declared where the
//! agent tool is registered — e.g. by the CLI), never hardcoded here.
//!
//! Cancellation is clean: the child's own ops live in a [`JoinSet`] that aborts
//! them all when the child future is dropped, so aborting the parent's agent task
//! tears down the whole subtree (no leaked work).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use hugr_core::{
    Brain, Command, Event, HookPhase, LogEntry, ModelSelector, OpId, OutputEvent, Record,
    StaticPolicy, SteerMode, Timestamp, Value,
};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::{AbortHandle, JoinSet};

use crate::capability::CapabilityRegistry;
use crate::engine::{
    AgentDefaults, Clock, estimate_text_tokens, estimate_value_tokens, missing_model_event,
    run_capability_op, run_model_op, tool_shaped_completion, unknown_capability_event,
};
use crate::model::ModelRegistry;
use crate::policy::Policy;

/// Everything a sub-agent run borrows from its host: the shared registries,
/// permission policy, per-kind registration defaults, and the nesting cap.
/// Cheap to clone (`Arc`s all the way down), so a child hands the same host
/// context to its own grandchildren.
#[derive(Clone)]
pub(crate) struct AgentHost {
    pub models: ModelRegistry,
    pub caps: CapabilityRegistry,
    pub policy: Arc<dyn Policy>,
    /// The logical model a child uses when neither its config nor its kind's
    /// registered defaults name one.
    pub default_model: ModelSelector,
    pub clock: Clock,
    /// Registration-time defaults per agent kind (ARCHITECTURE §13.1): the
    /// runner only *consumes* these; declaring them belongs to the embedder.
    pub defaults: Arc<HashMap<String, AgentDefaults>>,
    /// Maximum nested sub-agent depth. A spawn beyond the cap fails with a
    /// semantic error routed back to the calling model as the tool result.
    pub max_depth: usize,
}

/// Run a sub-agent to completion and report its result to the parent. `agent`
/// is the typed agent name from [`Command::StartAgent`]; `depth` is this
/// child's nesting depth (the root engine's children run at 1).
///
/// Returns a **boxed** future so a sub-agent can itself spawn sub-agents: a bare
/// `async fn` that spawned `run_agent` would have an infinitely recursive future
/// type. Boxing erases the type at the recursion point.
pub(crate) fn run_agent(
    op: OpId,
    agent: String,
    config: Value,
    seed: Vec<LogEntry>,
    depth: usize,
    host: AgentHost,
    parent_tx: UnboundedSender<Event>,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let event = match drive_agent(op, agent, config, seed, depth, host, &parent_tx).await {
            Ok(result) => Event::AgentDone {
                op,
                est_tokens: estimate_value_tokens(&result),
                result,
            },
            Err(error) => Event::AgentError {
                op,
                est_tokens: estimate_value_tokens(&error),
                error,
            },
        };
        // If the parent has gone away (its receiver dropped), there is nothing to
        // report to — the whole subtree is being torn down anyway.
        let _ = parent_tx.send(event);
    })
}

/// The child's runtime context, cloned into each spawned op task.
struct ChildCtx {
    host: AgentHost,
    /// The child brain's inbox (op tasks feed their result events here).
    tx: UnboundedSender<Event>,
    /// The parent brain's inbox — where the child forwards cosmetic progress.
    parent_tx: UnboundedSender<Event>,
    /// This agent's op id **in the parent**, used to tag forwarded progress.
    agent_op: OpId,
    /// This child's nesting depth; a grandchild spawns at `depth + 1`.
    depth: usize,
}

/// Build the child brain from its config + seed, drive it to idle, and return
/// its digest (`Ok`) or a semantic error (`Err`, surfaced as `AgentError` — it
/// folds back into the parent as an error tool result the model can react to).
async fn drive_agent(
    agent_op: OpId,
    agent: String,
    config: Value,
    seed: Vec<LogEntry>,
    depth: usize,
    host: AgentHost,
    parent_tx: &UnboundedSender<Event>,
) -> Result<Value, Value> {
    // --- explicit depth enforcement (ARCHITECTURE §13) ------------------------
    // Exceeding the cap is a *semantic* error: it routes back to the calling
    // model as the tool result (transport stays healthy; the model can adapt).
    if depth > host.max_depth {
        return Err(json!({
            "error": "agent_depth_exceeded",
            "message": format!(
                "sub-agent nesting depth {depth} exceeds the host cap of {}",
                host.max_depth
            ),
            "max_depth": host.max_depth,
        }));
    }

    // --- interpret the opaque config (the model's tool-call args, §13.1) ------
    let prompt = config
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            json!({ "error": "agent_config", "message": "sub-agent config needs a string `prompt`" })
        })?
        .to_string();
    // Registration-time defaults for this agent kind (declared where the agent
    // tool was registered). An unregistered kind gets the neutral fallback —
    // the host default model and every parent tool — matching what an agent
    // with no declared defaults always got.
    let defaults = host.defaults.get(&agent).cloned().unwrap_or_default();
    let selector = config
        .get("model")
        .and_then(|v| v.as_str())
        .map(ModelSelector::named)
        .or(defaults.model)
        .unwrap_or_else(|| host.default_model.clone());
    // Optional tool allowlist: the subset of the parent's tools the child may
    // use — the config's `tools` wins, then the kind's registered default.
    let allow: Option<HashSet<String>> = config
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .or_else(|| defaults.tools.map(|tools| tools.into_iter().collect()));
    let caps = host.caps.subset(allow.as_ref());
    // An allowlist that intersects the parent's registry to *zero* tools is a
    // semantic error, not a silently tool-less child (which would burn a whole
    // child turn producing nothing actionable).
    if allow.is_some() && caps.schemas().is_empty() {
        return Err(json!({
            "error": "agent_tools_empty",
            "message": "the sub-agent's `tools` allowlist matches none of the host's capabilities",
        }));
    }

    // --- assemble the child's policy from its (subset) tools ------------------
    // Depth handling: the child's policy registers **no agent tools** (there is
    // no `with_agent` here), so a child cannot spawn grandchildren today. That
    // is intentional but incidental — the explicit `depth > max_depth` check
    // above is the enforced invariant, so a future policy that *does* advertise
    // nested agents still cannot recurse past the host's cap.
    let mut child_policy = StaticPolicy::default()
        .with_model(selector)
        .with_tools(caps.schemas())
        .with_permissioned(caps.permissioned_names())
        .with_background(caps.background_names());
    if let Some(system) = config.get("system").and_then(|v| v.as_str()) {
        child_policy = child_policy.with_system_prompt(system);
    }

    // Seed the child brain from the forked log prefix (empty for `Fresh`).
    let mut brain = Brain::from_log(Box::new(child_policy), seed);

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let clock = host.clock.clone();
    let ctx = ChildCtx {
        host,
        tx: tx.clone(),
        parent_tx: parent_tx.clone(),
        agent_op,
        depth,
    };
    // Child ops live here; dropping the set aborts them all (clean subtree
    // teardown when this future is aborted by the parent's `Cancel`).
    let mut join: JoinSet<()> = JoinSet::new();
    let mut handles: HashMap<OpId, AbortHandle> = HashMap::new();

    // Kick off the child's first (and only injected) user turn.
    child_submit(
        &mut brain,
        &clock,
        Event::UserInput {
            content: json!(prompt),
            mode: SteerMode::Queue,
            est_tokens: estimate_text_tokens(&prompt),
        },
    );

    // The child driver loop — the same shape as the top-level engine loop, but
    // headless (no front-end / recorder — child ops are not recorded into the
    // parent trace) and feeding its own inbox. The engine's builtin
    // PreTool/PostTool hooks fire here too, folded into the *child's* log.
    loop {
        loop {
            let commands = brain.poll();
            if commands.is_empty() {
                break;
            }
            for command in commands {
                // Mirror the engine: every tool-shaped start fires the builtin
                // PreTool hook before dispatch.
                if let Some(payload) = pre_tool_payload(&command) {
                    child_fire_hook(
                        &mut brain,
                        &clock,
                        HookPhase::PreTool,
                        "builtin_pre_tool",
                        payload,
                    );
                }
                perform_child(&ctx, command, &mut join, &mut handles);
            }
        }
        if brain.state().inflight_len() == 0 {
            break;
        }
        match rx.recv().await {
            Some(event) => {
                // Mirror the engine: every tool-shaped completion fires the
                // builtin PostTool hook, off the same shared classification
                // (`tool_shaped_completion`) so the two can never diverge.
                let post_tool = tool_shaped_completion(&event).map(|(op, payload, is_error)| {
                    if is_error {
                        json!({ "op": op.0, "ok": false, "error": payload })
                    } else {
                        json!({ "op": op.0, "ok": true, "result": payload })
                    }
                });
                child_submit(&mut brain, &clock, event);
                if let Some(payload) = post_tool {
                    child_fire_hook(
                        &mut brain,
                        &clock,
                        HookPhase::PostTool,
                        "builtin_post_tool",
                        payload,
                    );
                }
            }
            None => break,
        }
    }

    // The child's digest: its last consolidated answer plus aggregated usage
    // (per-agent cost/latency attribution, §13.1). Forks diverge; a single value
    // flows back (§14.3).
    let text = brain
        .state()
        .log()
        .iter()
        .rev()
        .find_map(|e| match &e.record {
            Record::ModelOutput { output, .. } => Some(output.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let (input_tokens, output_tokens) = aggregate_usage(brain.state().log());
    Ok(json!({
        "text": text,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    }))
}

/// Inject a `Tick` (host clock) then the event — mirroring the engine so the
/// child's log entries are timestamped and the core stays clock-free.
fn child_submit(brain: &mut Brain, clock: &Clock, event: Event) {
    brain.submit(Event::Tick {
        now: Timestamp((clock)()),
    });
    brain.submit(event);
}

/// Fire one of the engine's builtin lifecycle hooks into the *child* brain
/// (mirroring `Engine::fire_hook`, minus the recorder/front-end the headless
/// child doesn't have).
fn child_fire_hook(brain: &mut Brain, clock: &Clock, phase: HookPhase, name: &str, result: Value) {
    let est_tokens = estimate_value_tokens(&result);
    child_submit(
        brain,
        clock,
        Event::HookFired {
            phase,
            name: name.to_string(),
            result,
            est_tokens,
        },
    );
}

/// The builtin PreTool hook payload for a tool-shaped command (capability or
/// sub-agent start), mirroring the engine's payloads. `None` for everything
/// else.
fn pre_tool_payload(command: &Command) -> Option<Value> {
    match command {
        Command::StartCapability { op, name, args } => {
            Some(json!({ "op": op.0, "capability": name, "args": args }))
        }
        Command::StartAgent {
            op, agent, config, ..
        } => Some(json!({ "op": op.0, "capability": format!("agent:{agent}"), "args": config })),
        _ => None,
    }
}

/// Perform one child command by spawning the appropriate op task (or forwarding
/// cosmetic output). Synchronous: every effect is a spawned task feeding the
/// child inbox, so the drain loop never blocks. Model and capability dispatch
/// reuse the engine's shared op runners (`run_model_op`/`run_capability_op`),
/// so the terminal-event construction exists exactly once.
fn perform_child(
    ctx: &ChildCtx,
    command: Command,
    join: &mut JoinSet<()>,
    handles: &mut HashMap<OpId, AbortHandle>,
) {
    match command {
        Command::StartModelCall { op, model, request } => match ctx.host.models.get(&model) {
            Some(adapter) => {
                let handle = join.spawn(run_model_op(adapter, op, request, ctx.tx.clone()));
                handles.insert(op, handle);
            }
            None => {
                let _ = ctx.tx.send(missing_model_event(op, &model));
            }
        },

        Command::StartCapability { op, name, args } => match ctx.host.caps.get(&name) {
            Some(capability) => {
                let handle = join.spawn(run_capability_op(capability, op, args, ctx.tx.clone()));
                handles.insert(op, handle);
            }
            None => {
                let _ = ctx.tx.send(unknown_capability_event(op, &name));
            }
        },

        // A grandchild: recurse at `depth + 1`. It feeds *this* child's inbox
        // and reports back via `AgentDone` keyed by its op — nesting works with
        // no special case, and the depth cap is enforced inside `drive_agent`.
        Command::StartAgent {
            op,
            agent,
            config,
            seed,
        } => {
            let handle = join.spawn(run_agent(
                op,
                agent,
                config,
                seed,
                ctx.depth + 1,
                ctx.host.clone(),
                ctx.tx.clone(),
            ));
            handles.insert(op, handle);
        }

        Command::RequestPermission { op, request } => {
            let tx = ctx.tx.clone();
            let policy = ctx.host.policy.clone();
            join.spawn(async move {
                let decision = policy.decide(&request).await;
                let est_tokens = crate::engine::permission_decision_est_tokens(&decision);
                let _ = tx.send(Event::PermissionDecision {
                    op,
                    decision,
                    est_tokens,
                });
            });
        }

        Command::AskUser { op, .. } => {
            // Sub-agents are non-interactive: there is no user at this layer.
            // Answer with a semantic error so the child's model can react.
            let answer = json!({ "error": "ask_user_unsupported_in_sub_agent" });
            let _ = ctx.tx.send(Event::UserAnswer {
                op,
                est_tokens: estimate_value_tokens(&answer),
                answer,
            });
        }

        Command::Cancel { op } => {
            if let Some(handle) = handles.remove(&op) {
                handle.abort();
            }
            let _ = ctx.tx.send(Event::OpCancelled { op });
        }

        // Forward the child's streamed assistant text to the parent as a cosmetic
        // chunk keyed by the agent op, so a front-end can show child progress.
        Command::Emit(OutputEvent::ModelText { text, .. }) => {
            let _ = ctx.parent_tx.send(Event::CapabilityChunk {
                op: ctx.agent_op,
                chunk: json!({ "agent_text": text }),
            });
        }

        // Other cosmetic output, checkpoints, and the child's own `Done` need no
        // action: the loop ends when the child brain goes idle.
        _ => {}
    }
}

/// Sum the input/output tokens across a child's ended ops (usage attribution).
fn aggregate_usage(log: &[LogEntry]) -> (u64, u64) {
    log.iter().fold((0, 0), |(input, output), entry| {
        if let Record::OpEnded { meta, .. } = &entry.record {
            if let Some(usage) = &meta.usage {
                return (input + usage.input_tokens, output + usage.output_tokens);
            }
        }
        (input, output)
    })
}
