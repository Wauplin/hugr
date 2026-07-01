//! Phase 0 exit criterion #1: a scripted "user → model call → tool call →
//! model call → done" session reduces to the expected command sequence.

mod common;

use common::*;
use hugr_core::{
    Brain, Command, ContextDisposition, ContextSource, DoneReason, Event, ModelSelector, OpId,
    StaticPolicy, TokenBudget, ToolSchema, TurnPolicy,
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
                version: None,
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

/// The same session, but the tool requires permission. The sequence gains a
/// `RequestPermission` before the capability actually starts, and the granted
/// capability reuses the same op id.
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
                version: None,
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
                version: None,
                est_tokens: 1,
            },
            // Second tool finishes — now the model resumes.
            Event::CapabilityDone {
                op: OpId(2),
                result: json!({ "status": 200 }),
                version: None,
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
    assert_eq!(plan.totals.included_tokens, 4);
    assert_eq!(plan.totals.omitted_tokens, 0);
    assert!(plan.cache_hints.is_empty());

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
