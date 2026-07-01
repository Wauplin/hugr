//! The sub-agent runner (ARCHITECTURE §13).
//!
//! A sub-agent is **not a special subsystem** — it is *another
//! [`hugr_core::Brain`]* the host drives to completion on its own task, exactly
//! like the top-level [`Engine`](crate::Engine) drives the root brain. Because
//! the core is tiny, pure and runtime-free, spawning one is cheap, and an
//! arbitrarily deep tree of agents is just a tree of brains.
//!
//! This module implements the **in-process** isolation mode (§13.2): the child
//! runs on a spawned tokio task, reusing (a subset of) the parent's model and
//! capability registries. Its progress streams back to the parent as ordinary
//! [`Event`]s keyed by the parent's op, and its final digest returns as
//! [`Event::AgentDone`] (§13.1) — the parent folds it like any other op result.
//!
//! Cancellation is clean: the child's own ops live in a [`JoinSet`] that aborts
//! them all when the child future is dropped, so aborting the parent's agent task
//! tears down the whole subtree (no leaked work).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use hugr_core::{
    Brain, Command, Event, LogEntry, ModelSelector, OpId, OutputEvent, Record, StaticPolicy,
    SteerMode, Timestamp, Value,
};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::{AbortHandle, JoinSet};

use crate::capability::{CapabilityRegistry, ChunkSink};
use crate::engine::{
    Clock, estimate_text_tokens, estimate_value_tokens, model_output_est_tokens,
    permission_decision_est_tokens,
};
use crate::model::{ModelRegistry, ModelSink};
use crate::policy::Policy;

/// Run a sub-agent to completion and report its result to the parent.
///
/// Returns a **boxed** future so a sub-agent can itself spawn sub-agents: a bare
/// `async fn` that spawned `run_agent` would have an infinitely recursive future
/// type. Boxing erases the type at the recursion point.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_agent(
    op: OpId,
    config: Value,
    seed: Vec<LogEntry>,
    models: ModelRegistry,
    caps: CapabilityRegistry,
    policy: Arc<dyn Policy>,
    default_model: ModelSelector,
    clock: Clock,
    parent_tx: UnboundedSender<Event>,
) -> Pin<Box<dyn Future<Output = ()> + Send>> {
    Box::pin(async move {
        let event = match drive_agent(
            op,
            config,
            seed,
            models,
            caps,
            policy,
            default_model,
            clock,
            &parent_tx,
        )
        .await
        {
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
    models: ModelRegistry,
    caps: CapabilityRegistry,
    policy: Arc<dyn Policy>,
    default_model: ModelSelector,
    clock: Clock,
    /// The child brain's inbox (op tasks feed their result events here).
    tx: UnboundedSender<Event>,
    /// The parent brain's inbox — where the child forwards cosmetic progress.
    parent_tx: UnboundedSender<Event>,
    /// This agent's op id **in the parent**, used to tag forwarded progress.
    agent_op: OpId,
}

/// Build the child brain from its config + seed, drive it to idle, and return
/// its digest (`Ok`) or a semantic error (`Err`, surfaced as `AgentError`).
#[allow(clippy::too_many_arguments)]
async fn drive_agent(
    agent_op: OpId,
    config: Value,
    seed: Vec<LogEntry>,
    models: ModelRegistry,
    caps: CapabilityRegistry,
    policy: Arc<dyn Policy>,
    default_model: ModelSelector,
    clock: Clock,
    parent_tx: &UnboundedSender<Event>,
) -> Result<Value, Value> {
    // --- interpret the opaque config (the model's tool-call args, §13.1) ------
    let prompt = config
        .get("prompt")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            json!({ "error": "agent_config", "message": "sub-agent config needs a string `prompt`" })
        })?
        .to_string();
    let agent_kind = config
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or("task");
    let selector = config
        .get("model")
        .and_then(|v| v.as_str())
        .map(ModelSelector::named)
        .or_else(|| default_model_for_agent(agent_kind).map(ModelSelector::named))
        .unwrap_or(default_model.clone());
    // Optional tool allowlist: the subset of the parent's tools the child may use.
    let allow: Option<HashSet<String>> = config
        .get("tools")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .or_else(|| default_tools_for_agent(agent_kind));
    let caps = caps.subset(allow.as_ref());

    // --- assemble the child's policy from its (subset) tools ------------------
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
    let ctx = ChildCtx {
        models,
        caps,
        policy,
        default_model,
        clock: clock.clone(),
        tx: tx.clone(),
        parent_tx: parent_tx.clone(),
        agent_op,
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
    // headless (no front-end / recorder) and feeding its own inbox.
    loop {
        loop {
            let commands = brain.poll();
            if commands.is_empty() {
                break;
            }
            for command in commands {
                perform_child(&ctx, command, &mut join, &mut handles);
            }
        }
        if brain.state().inflight_len() == 0 {
            break;
        }
        match rx.recv().await {
            Some(event) => child_submit(&mut brain, &clock, event),
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

fn default_model_for_agent(agent: &str) -> Option<&'static str> {
    match agent {
        "explorer" => Some("small"),
        "implementer" | "reviewer" | "test_fixer" => Some("big"),
        _ => None,
    }
}

fn default_tools_for_agent(agent: &str) -> Option<HashSet<String>> {
    let tools = match agent {
        "explorer" => &[
            "repo_files",
            "repo_search",
            "repo_read",
            "git_status",
            "git_log",
            "package_metadata",
        ][..],
        "implementer" => &[
            "repo_read",
            "repo_search",
            "fs_read",
            "fs_write",
            "patch_apply",
            "cargo_verify",
            "git_diff",
            "git_status",
        ][..],
        "reviewer" => &[
            "repo_read",
            "repo_search",
            "git_diff",
            "git_status",
            "cargo_verify",
        ][..],
        "test_fixer" => &[
            "repo_read",
            "repo_search",
            "fs_read",
            "fs_write",
            "patch_apply",
            "cargo_verify",
            "git_diff",
        ][..],
        _ => return None,
    };
    Some(tools.iter().map(|tool| (*tool).to_string()).collect())
}

/// Perform one child command by spawning the appropriate op task (or forwarding
/// cosmetic output). Synchronous: every effect is a spawned task feeding the
/// child inbox, so the drain loop never blocks.
fn perform_child(
    ctx: &ChildCtx,
    command: Command,
    join: &mut JoinSet<()>,
    handles: &mut HashMap<OpId, AbortHandle>,
) {
    match command {
        Command::StartModelCall { op, model, request } => {
            let tx = ctx.tx.clone();
            match ctx.models.get(&model) {
                Some(adapter) => {
                    let handle = join.spawn(async move {
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
                    handles.insert(op, handle);
                }
                None => {
                    let _ = tx.send(Event::ModelError {
                        op,
                        error: json!({ "message": format!("no adapter for model {model:?}") }),
                    });
                }
            }
        }

        Command::StartCapability { op, name, args } => {
            let tx = ctx.tx.clone();
            match ctx.caps.get(&name) {
                Some(capability) => {
                    let handle = join.spawn(async move {
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
                    handles.insert(op, handle);
                }
                None => {
                    let error = json!({ "error": format!("unknown capability: {name}") });
                    let _ = tx.send(Event::CapabilityError {
                        op,
                        est_tokens: estimate_value_tokens(&error),
                        error,
                        conflict: None,
                    });
                }
            }
        }

        // A grandchild: recurse. It feeds *this* child's inbox and reports back
        // via `AgentDone` keyed by its op — nesting works with no special case.
        Command::StartAgent { op, config, seed } => {
            let handle = join.spawn(run_agent(
                op,
                config,
                seed,
                ctx.models.clone(),
                ctx.caps.clone(),
                ctx.policy.clone(),
                ctx.default_model.clone(),
                ctx.clock.clone(),
                ctx.tx.clone(),
            ));
            handles.insert(op, handle);
        }

        Command::RequestPermission { op, request } => {
            let tx = ctx.tx.clone();
            let policy = ctx.policy.clone();
            join.spawn(async move {
                let decision = policy.decide(&request).await;
                let est_tokens = permission_decision_est_tokens(&decision);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coding_agent_defaults_constrain_tools_and_tiers() {
        assert_eq!(default_model_for_agent("explorer"), Some("small"));
        assert_eq!(default_model_for_agent("reviewer"), Some("big"));
        let reviewer = default_tools_for_agent("reviewer").expect("reviewer tools");
        assert!(reviewer.contains("git_diff"));
        assert!(reviewer.contains("cargo_verify"));
        assert!(!reviewer.contains("fs_write"));
        let implementer = default_tools_for_agent("implementer").expect("implementer tools");
        assert!(implementer.contains("patch_apply"));
        assert!(implementer.contains("fs_write"));
    }
}
