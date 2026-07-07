//! Phase 0 exit criterion #1: a scripted "user → model call → tool call →
//! model call → done" session reduces to the expected command sequence.

mod common;

use common::*;
use hugr_core::{
    Brain, Command, ContentPart, ContextDisposition, ContextSource, DoneReason, Event,
    ModelSelector, OpId, Record, Role, StaticPolicy, TokenBudget, ToolSchema, TurnPolicy,
};
use serde_json::json;

/// The canonical turn loop: one user message, one tool round-trip, one final
/// answer. Assert the exact effectful command sequence and the op ids that
/// correlate commands to their results.
#[test]
fn user_model_tool_model_done() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            // 1. User asks something → brain starts a model call (op 0).
            user("list the files"),
            // 2. Model wants to call a tool (op 0 result) → brain starts the
            //    capability (op 1).
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "ls" })),
                usage: usage(),
                est_tokens: 1,
            },
            // 3. Tool finishes (op 1 result) → brain calls the model again (op 2).
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "stdout": "a.txt\nb.txt" }),
                est_tokens: 1,
            },
            // 4. Model gives a final answer with no tool calls → turn is done.
            Event::ModelDone {
                op: OpId(2),
                output: text_output("There are two files: a.txt and b.txt."),
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
                Command::StartCapability { op: OpId(1), name, .. },
                Command::StartModelCall { op: OpId(2), .. },
                Command::Checkpoint,
                Command::Done { reason: DoneReason::EndTurn },
            ] if name == "shell"
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    let tokens: Vec<u32> = brain
        .state()
        .log()
        .iter()
        .filter_map(|entry| match &entry.record {
            hugr_core::Record::UserMessage { est_tokens, .. }
            | hugr_core::Record::ModelOutput { est_tokens, .. }
            | hugr_core::Record::ToolResult { est_tokens, .. } => Some(*est_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(tokens, vec![1, 1, 1, 1]);
}

#[test]
fn tool_results_are_projected_adjacent_to_tool_calls_even_with_records_between() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("list the files"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "ls" })),
                usage: usage(),
                est_tokens: 1,
            },
            // A durable record lands between the model output and the tool
            // result (here: a queued mid-turn user message) — projection must
            // still group the tool result adjacent to its originating call.
            Event::UserInput {
                content: json!("also check the README"),
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "stdout": "a.txt\nb.txt" }),
                est_tokens: 1,
            },
        ],
    );

    let followup = commands
        .iter()
        .rev()
        .filter_map(|cmd| match cmd {
            Command::StartModelCall { request, .. } => Some(request),
            _ => None,
        })
        .next()
        .expect("follow-up model request");

    let roles: Vec<Role> = followup.blocks.iter().map(|block| block.role).collect();
    assert_eq!(roles[..3], [Role::User, Role::Assistant, Role::Tool]);
    assert!(matches!(
        followup.blocks[1].content.as_slice(),
        [ContentPart::ToolUse { id, .. }] if id == "call-1"
    ));
    assert!(matches!(
        followup.blocks[2].content.as_slice(),
        [ContentPart::ToolResult { id, .. }] if id == "call-1"
    ));
}

#[test]
fn permissioned_tool_round_trip() {
    let policy = StaticPolicy::default().with_permissioned(["shell".to_string()]);
    let mut brain = Brain::new(Box::new(policy));

    let commands = run_script(
        &mut brain,
        vec![
            user("delete everything"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "rm -rf ." })),
                usage: usage(),
                est_tokens: 1,
            },
            // Brain asked for permission on op 1; the policy grants it.
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Allow,
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "ok": true }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Done."),
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
                Command::RequestPermission { op: OpId(1), .. },
                Command::StartCapability { op: OpId(1), .. },
                Command::StartModelCall { op: OpId(2), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}

/// A denied permission feeds an error result back to the model rather than
/// running the tool; the turn then continues.
#[test]
fn denied_permission_feeds_error_back() {
    let policy = StaticPolicy::default().with_permissioned(["shell".to_string()]);
    let mut brain = Brain::new(Box::new(policy));

    let commands = run_script(
        &mut brain,
        vec![
            user("delete everything"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "rm -rf ." })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Deny {
                    reason: "too dangerous".to_string(),
                },
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Understood, I won't."),
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
                Command::RequestPermission { op: OpId(1), .. },
                // No StartCapability — the model is re-prompted with the denial.
                Command::StartModelCall { op: OpId(2), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}

/// Two tool calls in one model turn run concurrently; the brain only resumes
/// the model once *both* have resolved (ARCHITECTURE §6.3 fan-out).
#[test]
fn parallel_tool_calls_resume_once() {
    use hugr_core::{ModelOutput, ToolCall};

    let mut brain = Brain::with_default_policy();

    let two_calls = ModelOutput::tool_calls(vec![
        ToolCall::new("a", "shell", json!({ "cmd": "ls" })),
        ToolCall::new("b", "http", json!({ "url": "https://example.com" })),
    ]);

    let commands = run_script(
        &mut brain,
        vec![
            user("do two things"),
            Event::ModelDone {
                op: OpId(0),
                output: two_calls,
                usage: usage(),
                est_tokens: 1,
            },
            // First tool finishes — must NOT resume the model yet.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "stdout": "x" }),
                est_tokens: 1,
            },
            // Second tool finishes — now the model resumes.
            Event::CapabilityDone {
                op: OpId(2),
                result: json!({ "status": 200 }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(3),
                output: text_output("All done."),
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
                Command::StartCapability { op: OpId(1), .. },
                Command::StartCapability { op: OpId(2), .. },
                // Exactly one resume after both tools resolved.
                Command::StartModelCall { op: OpId(3), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}

/// The projected request carries the policy's advertised tools and the
/// conversation rendered from the log (trivial pass-through projection).
#[test]
fn projection_includes_tools_and_history() {
    let tools = vec![ToolSchema::new(
        "shell",
        "run a shell command",
        json!({ "type": "object" }),
    )];
    let policy = StaticPolicy::default()
        .with_model(ModelSelector::named("fast"))
        .with_tools(tools);
    let mut brain = Brain::new(Box::new(policy));

    brain.submit(user("hello"));
    let commands = brain.poll();

    let Command::StartModelCall { model, request, .. } = &commands[0] else {
        panic!("expected a StartModelCall, got {commands:#?}");
    };
    assert_eq!(*model, ModelSelector::named("fast"));
    assert_eq!(request.tools.len(), 1);
    assert_eq!(request.tools[0].name, "shell");
    // The one user message is projected into exactly one context block.
    assert_eq!(request.blocks.len(), 1);
}

/// A1: projection returns an inspectable plan first; the request is derived from
/// included/reference/summary entries rather than being built directly.
#[test]
fn context_plan_explains_dispositions_and_renders_request() {
    let mut brain = Brain::with_default_policy();

    run_script(
        &mut brain,
        vec![
            user("hello"),
            Event::ModelDone {
                op: OpId(0),
                output: text_output("hi"),
                usage: usage(),
                est_tokens: 3,
            },
        ],
    );

    let policy = StaticPolicy::default();
    let budget = TokenBudget::new(42);
    let plan = policy.project_context(brain.state().log(), budget);
    let request = plan.to_model_request();

    assert_eq!(plan.budget, budget);
    assert_eq!(plan.entries.len(), 3);
    assert_eq!(request.blocks.len(), 2);
    assert_eq!(plan.totals.used_tokens, 4);
    assert_eq!(plan.totals.omitted_tokens, 0);

    assert!(
        matches!(plan.entries[0].source, ContextSource::LogEntry { .. })
            && matches!(
                plan.entries[0].disposition,
                ContextDisposition::Included { .. }
            )
            && plan.entries[0].reason == "static pass-through projection"
    );
    assert!(
        matches!(plan.entries[1].source, ContextSource::LogEntry { .. })
            && matches!(
                plan.entries[1].disposition,
                ContextDisposition::Included { .. }
            )
    );
    assert!(
        matches!(plan.entries[2].source, ContextSource::LogEntry { .. })
            && matches!(plan.entries[2].disposition, ContextDisposition::Omitted)
            && plan.entries[2].reason == "operation metadata is not model context"
    );
}

#[test]
fn duplicate_permission_allow_does_not_drop_the_live_op() {
    let policy = StaticPolicy::default().with_permissioned(["shell".to_string()]);
    let mut brain = Brain::new(Box::new(policy));

    let commands = run_script(
        &mut brain,
        vec![
            user("delete everything"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "rm -rf ." })),
                usage: usage(),
                est_tokens: 1,
            },
            // Grant permission: op 1 starts the capability.
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Allow,
                est_tokens: 1,
            },
            // A stray DUPLICATE Allow for the now-running op 1: must be ignored,
            // NOT remove the live capability op.
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Allow,
                est_tokens: 1,
            },
            // The capability still resolves normally.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "ok": true }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Done."),
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
                Command::RequestPermission { op: OpId(1), .. },
                // The capability starts exactly once (the first Allow); the
                // duplicate produced NO second StartCapability.
                Command::StartCapability { op: OpId(1), .. },
                Command::StartModelCall { op: OpId(2), .. },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ]
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // Exactly one ToolResult for call-1 (the real capability result), and it
    // is the success payload — the duplicate Allow neither dropped the op nor
    // synthesized a spurious result.
    let tool_results: Vec<_> = brain
        .state()
        .log()
        .iter()
        .filter(|e| matches!(&e.record, Record::ToolResult { call_id, .. } if call_id == "call-1"))
        .collect();
    assert_eq!(tool_results.len(), 1);
}

/// Replay: the duplicate-Allow script re-fed to a fresh brain yields identical
/// commands and an identical log (ARCHITECTURE §6.2).
#[test]
fn duplicate_permission_allow_replay_is_deterministic() {
    let make_brain = || {
        Brain::new(Box::new(
            StaticPolicy::default().with_permissioned(["shell".to_string()]),
        ))
    };
    let script = || {
        vec![
            user("delete everything"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "rm -rf ." })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Allow,
                est_tokens: 1,
            },
            Event::PermissionDecision {
                op: OpId(1),
                decision: hugr_core::Decision::Allow,
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "ok": true }),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Done."),
                usage: usage(),
                est_tokens: 1,
            },
        ]
    };
    assert_deterministic_replay(make_brain, script);
}

/// A plain model transport error ends the turn `Done(Error)` with the error
/// stringified as the reason (ARCHITECTURE §5.4). `Event::ModelError` had zero
/// coverage before; this pins its command sequence.
#[test]
fn model_error_ends_the_turn_with_error() {
    let mut brain = Brain::with_default_policy();

    let commands = run_script(
        &mut brain,
        vec![
            user("hi"),
            Event::ModelError {
                op: OpId(0),
                error: json!("upstream 503"),
            },
        ],
    );

    let effectful = effectful(&commands);
    assert!(
        matches!(
            effectful.as_slice(),
            [
                Command::StartModelCall { op: OpId(0), .. },
                Command::Done {
                    reason: DoneReason::Error(reason)
                },
            ] if reason == "upstream 503"
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The error is folded into the durable log as the op's Error outcome.
    assert!(brain.state().log().iter().any(|e| matches!(
        &e.record,
        Record::OpEnded {
            op: OpId(0),
            outcome: hugr_core::OpOutcome::Error(_),
            ..
        }
    )));
}

/// A model error while a **background** op is still running must defer the
/// terminal `Done(Error)` (mirroring `on_model_done`'s deferral, ARCHITECTURE
/// §4.2): the turn is not over while work is in flight. Once the background op
/// drains, `Done(Error)` fires with the original reason — never a
/// `StartModelCall` after a terminal `Done`.
#[test]
fn model_error_with_a_background_op_defers_done() {
    let policy = StaticPolicy::default().with_background(["shell".to_string()]);
    let mut brain = Brain::new(Box::new(policy));

    let commands = run_script(
        &mut brain,
        vec![
            user("build and chat"),
            // The model kicks off a background shell (op 1); the turn resumes
            // into a second model call (op 2) concurrently.
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            // The resumed model call (op 2) errors while the shell still runs.
            // Done is DEFERRED — the background op is still in flight.
            Event::ModelError {
                op: OpId(2),
                error: json!("stream reset"),
            },
            // The background shell finishes: now the deferred error resolves.
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
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
                Command::StartCapability { op: OpId(1), .. },
                Command::StartModelCall { op: OpId(2), .. },
                // No Done right after the error — it is deferred.
                // The background op draining resolves it (NOT a resume).
                Command::Done {
                    reason: DoneReason::Error(reason)
                },
            ] if reason == "stream reset"
        ),
        "unexpected command sequence: {effectful:#?}"
    );
}

/// Replay: the ModelError scripts re-fed to fresh brains yield identical
/// commands and identical logs (ARCHITECTURE §6.2).
#[test]
fn model_error_replay_is_deterministic() {
    let plain = || {
        vec![
            user("hi"),
            Event::ModelError {
                op: OpId(0),
                error: json!("upstream 503"),
            },
        ]
    };
    assert_deterministic_replay(Brain::with_default_policy, plain);

    let deferred = || {
        vec![
            user("build and chat"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "cargo build" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::ModelError {
                op: OpId(2),
                error: json!("stream reset"),
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "exit_code": 0 }),
                est_tokens: 1,
            },
        ]
    };
    assert_deterministic_replay(
        || {
            Brain::new(Box::new(
                StaticPolicy::default().with_background(["shell".to_string()]),
            ))
        },
        deferred,
    );
}
