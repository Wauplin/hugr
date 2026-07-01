//! Phase 6: sub-agents & forks in the pure core.
//!
//! A sub-agent is just an op: when the policy designates a capability as an
//! agent spawner ([`TurnPolicy::agent_seed`]), the brain emits
//! [`Command::StartAgent`] (carrying a **forked** log prefix) instead of
//! `StartCapability`, and folds the child's [`Event::AgentDone`] result back
//! into the turn loop like any other op result (ARCHITECTURE §13/§14).

mod common;

use common::*;
use hugr_core::{
    AgentSeed, Brain, Command, DoneReason, Event, LogEntry, OpId, Record, StaticPolicy,
};
use serde_json::json;

/// Build a brain whose `task` capability spawns a sub-agent seeded per `seed`.
fn agent_brain(seed: AgentSeed) -> Brain {
    let policy = StaticPolicy::default().with_agent("task", seed);
    Brain::new(Box::new(policy))
}

/// The seed a `StartAgent` command carried (its forked log prefix).
fn start_agent_seed(commands: &[Command]) -> &[LogEntry] {
    commands
        .iter()
        .find_map(|c| match c {
            Command::StartAgent { seed, .. } => Some(seed.as_slice()),
            _ => None,
        })
        .expect("expected a StartAgent command")
}

/// The headline flow: model delegates to a sub-agent, the child's result folds
/// back, and a final model turn ends the session.
#[test]
fn model_delegates_to_sub_agent_and_folds_result() {
    let mut brain = agent_brain(AgentSeed::ForkFull);

    let commands = run_script(
        &mut brain,
        vec![
            user("delegate the work"),
            // The model calls the `task` agent tool (op 0 result).
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "task", json!({ "prompt": "do the thing" })),
                usage: usage(),
                est_tokens: 1,
            },
            // The child returns a digest (op 1) → folds back as a tool result.
            Event::AgentDone {
                op: OpId(1),
                result: json!({ "text": "child did the thing", "usage": { "input_tokens": 5, "output_tokens": 7 } }),
                est_tokens: 1,
            },
            // The parent model gives a final answer → turn done.
            Event::ModelDone {
                op: OpId(2),
                output: text_output("The sub-agent finished."),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                // A sub-agent, NOT a StartCapability.
                Command::StartAgent { op: OpId(1), .. },
                Command::StartModelCall { op: OpId(2), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The child's opaque config keeps the model's args and records agent
    // metadata for host-side defaults / trace-visible usage.
    let config = commands
        .iter()
        .find_map(|c| match c {
            Command::StartAgent { config, .. } => Some(config.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        config,
        json!({ "prompt": "do the thing", "agent": "task", "max_depth": 1 })
    );

    // The child's digest was folded into the parent log as a tool result,
    // correlated to the originating `tool_call` id (`call-1`, not the op id).
    let tool_result = brain.state().log().iter().find_map(|e| match &e.record {
        Record::ToolResult {
            name,
            call_id,
            result,
            ..
        } if name == "task" => Some((call_id.clone(), result.clone())),
        _ => None,
    });
    let (call_id, result) = tool_result.expect("the sub-agent result should be a tool result");
    assert_eq!(call_id, "call-1");
    assert_eq!(result["text"], json!("child did the thing"));
}

/// `ForkFull` seeds the child with the parent's whole log at spawn time; the
/// prefix is the shared context the child brain re-folds (ARCHITECTURE §14).
#[test]
fn fork_full_seeds_the_child_with_the_parent_log() {
    let mut brain = agent_brain(AgentSeed::ForkFull);

    let commands = run_script(
        &mut brain,
        vec![
            user("delegate the work"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "task", json!({ "prompt": "child" })),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    // At spawn time the log held: the user message, the model output, and the
    // model op's OpEnded — all three are copied into the child's seed.
    let seed = start_agent_seed(&commands);
    assert_eq!(seed.len(), 3, "ForkFull copies the whole log prefix");
    assert!(matches!(seed[0].record, Record::UserMessage { .. }));
    assert!(matches!(seed[1].record, Record::ModelOutput { .. }));
    assert!(matches!(seed[2].record, Record::OpEnded { .. }));
}

/// `ForkAt { seq }` copies only the prefix up to and including `seq` — the
/// branch/rewind primitive (a child that shares just early context).
#[test]
fn fork_at_seeds_only_the_prefix() {
    let mut brain = agent_brain(AgentSeed::ForkAt { seq: 0 });

    let commands = run_script(
        &mut brain,
        vec![
            user("delegate the work"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "task", json!({ "prompt": "child" })),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    let seed = start_agent_seed(&commands);
    assert_eq!(seed.len(), 1, "ForkAt {{ seq: 0 }} copies only seq 0");
    assert!(matches!(seed[0].record, Record::UserMessage { .. }));
}

/// `Fresh` gives the child an empty log — a fully isolated sub-agent.
#[test]
fn fresh_seeds_an_empty_child() {
    let mut brain = agent_brain(AgentSeed::Fresh);

    let commands = run_script(
        &mut brain,
        vec![
            user("delegate the work"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "task", json!({ "prompt": "child" })),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    assert!(
        start_agent_seed(&commands).is_empty(),
        "Fresh seeds an isolated (empty) child"
    );
}

/// A parent that fans out to two sub-agents in one turn resumes the model only
/// once **both** children have returned (the fan-out join, §6.3) — and the whole
/// thing replays deterministically (identical commands *and* log).
#[test]
fn fan_out_joins_and_replays_deterministically() {
    use hugr_core::{ModelOutput, ToolCall};

    let script = || {
        vec![
            user("fan out to two workers"),
            Event::ModelDone {
                op: OpId(0),
                output: ModelOutput::tool_calls(vec![
                    ToolCall::new("a", "task", json!({ "prompt": "first" })),
                    ToolCall::new("b", "task", json!({ "prompt": "second" })),
                ]),
                usage: usage(),
                est_tokens: 1,
            },
            // First child returns — must NOT resume the model yet.
            Event::AgentDone {
                op: OpId(1),
                result: json!({ "text": "first done" }),
                est_tokens: 1,
            },
            // Second child returns — now the model resumes.
            Event::AgentDone {
                op: OpId(2),
                result: json!({ "text": "second done" }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(3),
                output: text_output("Both workers finished."),
                usage: usage(),
                est_tokens: 1,
            },
        ]
    };

    let mut brain = agent_brain(AgentSeed::ForkFull);
    let commands = run_script(&mut brain, script());
    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::StartAgent { op: OpId(1), .. },
                Command::StartAgent { op: OpId(2), .. },
                // Exactly one resume, after BOTH children resolved.
                Command::StartModelCall { op: OpId(3), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // Determinism (ARCHITECTURE §6.3): re-feed the same events into a fresh brain
    // → identical commands and identical log.
    let mut replay = agent_brain(AgentSeed::ForkFull);
    let replay_commands = run_script(&mut replay, script());
    assert_eq!(commands, replay_commands, "commands must be deterministic");
    assert_eq!(
        brain.state().log(),
        replay.state().log(),
        "the folded log must be deterministic"
    );
}
