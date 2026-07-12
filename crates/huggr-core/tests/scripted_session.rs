//! Phase 0 exit criterion #1: a scripted "user → model call → tool call →
//! model call → done" session reduces to the expected command sequence.

mod common;

use std::collections::BTreeMap;

use common::*;
use huggr_core::{
    Brain, BudgetPolicy, Command, ContentPart, ContextDisposition, ContextSource, DoneReason,
    Event, ModelSelector, OpId, PolicyRegistry, Record, Role, StaticPolicy, TokenBudget,
    ToolSchema, TurnPolicy,
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
            huggr_core::Record::UserMessage { est_tokens, .. }
            | huggr_core::Record::ModelOutput { est_tokens, .. }
            | huggr_core::Record::ToolResult { est_tokens, .. } => Some(*est_tokens),
            _ => None,
        })
        .collect();
    assert_eq!(tokens, vec![1, 1, 1, 1]);
}

#[test]
fn wrong_kind_and_mid_turn_events_are_ignored() {
    let mut brain = Brain::with_default_policy();
    brain.submit(user("first"));
    let start = brain.poll();
    assert!(matches!(
        start.as_slice(),
        [Command::StartModelCall { op: OpId(0), .. }]
    ));
    let baseline = brain.state().log().len();

    brain.submit(Event::CapabilityDone {
        op: OpId(0),
        result: json!({ "wrong": true }),
        est_tokens: 1,
    });
    brain.submit(user("must not be stranded"));
    brain.submit(Event::ModelDone {
        op: OpId(99),
        output: text_output("stale"),
        usage: usage(),
        est_tokens: 1,
    });

    assert_eq!(brain.state().log().len(), baseline);
    assert_eq!(brain.state().inflight_len(), 1);
    assert!(brain.poll().is_empty());
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
                decision: huggr_core::Decision::Allow,
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
                decision: huggr_core::Decision::Deny {
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
/// the model once *both* have resolved.
#[test]
fn parallel_tool_calls_resume_once() {
    use huggr_core::{ModelOutput, ToolCall};

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
    );
}

#[test]
fn budget_policy_compacts_old_context_deterministically() {
    let mut brain = Brain::with_default_policy();
    run_script(
        &mut brain,
        vec![
            user("old user block with enough text to be truncated"),
            Event::ModelDone {
                op: OpId(0),
                output: text_output("old answer block with enough text to be truncated"),
                usage: usage(),
                est_tokens: 16,
            },
            user("recent question"),
            Event::ModelDone {
                op: OpId(1),
                output: text_output("recent answer"),
                usage: usage(),
                est_tokens: 2,
            },
        ],
    );

    let policy = BudgetPolicy::new(12)
        .with_trigger_tokens(12)
        .with_keep_recent_tokens(5)
        .with_max_block_tokens(3);
    let plan = policy.project_context(brain.state().log(), TokenBudget::new(12));
    let request = plan.to_model_request();

    assert!(plan.totals.used_tokens <= 12);
    assert!(plan.totals.dropped_tokens > 0 || plan.totals.truncated_tokens > 0);
    assert!(plan.entries.iter().any(|entry| matches!(
        entry.disposition,
        ContextDisposition::Dropped { .. } | ContextDisposition::Truncated { .. }
    )));
    assert!(request.blocks.iter().any(|block| {
        block.role == Role::System
            && block.content.iter().any(|part| {
                matches!(
                    part,
                    ContentPart::Text(text) if text.contains("Context compacted")
                )
            })
    }));
    assert!(
        request
            .blocks
            .iter()
            .any(|block| block.content.iter().any(|part| {
                matches!(part, ContentPart::Text(text) if text.contains("recent answer"))
            }))
    );
}

#[test]
fn budget_policy_forget_rules_drop_stale_tool_transcripts() {
    let mut brain = Brain::with_default_policy();
    run_script(
        &mut brain,
        vec![
            user("capture the page"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "page_snapshot", json!({})),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "text": "old page" }),
                est_tokens: 8,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("captured"),
                usage: usage(),
                est_tokens: 1,
            },
            user("capture it again"),
            Event::ModelDone {
                op: OpId(3),
                output: tool_output("call-2", "page_snapshot", json!({})),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(4),
                result: json!({ "text": "fresh page" }),
                est_tokens: 8,
            },
        ],
    );

    let policy = BudgetPolicy::new(100)
        .with_trigger_tokens(100)
        .with_keep_last_per_tool(BTreeMap::from([("page_snapshot".to_string(), 1)]));
    let plan = policy.project_context(brain.state().log(), TokenBudget::new(100));
    let request = plan.to_model_request();

    assert!(
        plan.entries
            .iter()
            .any(|entry| matches!(entry.disposition, ContextDisposition::Dropped { .. }))
    );
    assert!(!request.blocks.iter().any(|block| block.content.iter().any(|part| {
        matches!(part, ContentPart::ToolUse { id, .. } if id == "call-1")
            || matches!(part, ContentPart::ToolResult { id, .. } if id == "call-1")
            || matches!(part, ContentPart::ToolResult { result, .. } if result["text"] == "old page")
    })));
    assert!(request.blocks.iter().any(|block| block.content.iter().any(|part| {
        matches!(part, ContentPart::ToolUse { id, .. } if id == "call-2")
            || matches!(part, ContentPart::ToolResult { result, .. } if result["text"] == "fresh page")
    })));
}

#[test]
fn policy_registry_decodes_budget_policy() {
    let value = serde_json::json!({
        "kind": "budget",
        "budget_tokens": 32,
        "trigger_tokens": 40,
        "keep_recent_tokens": 8,
        "max_block_tokens": 4
    });
    let registry = PolicyRegistry::default();
    assert!(
        registry.decode(&value).is_some(),
        "built-in registry decodes budget policies"
    );
}

#[test]
fn budget_policy_requests_summary_before_main_model_turn() {
    let mut seed = Brain::with_default_policy();
    run_script(
        &mut seed,
        vec![
            Event::UserInput {
                content: json!("old user details that should be summarized"),
                est_tokens: 20,
            },
            Event::ModelDone {
                op: OpId(0),
                output: text_output("old answer details that should be summarized"),
                usage: usage(),
                est_tokens: 20,
            },
        ],
    );

    let policy = BudgetPolicy::new(12)
        .with_trigger_tokens(12)
        .with_keep_recent_tokens(2)
        .with_summary_selector(ModelSelector::named("summarizer"));
    let mut brain = Brain::from_log(Box::new(policy), seed.state().log().to_vec());
    let commands = run_script(
        &mut brain,
        vec![Event::UserInput {
            content: json!("new question"),
            est_tokens: 1,
        }],
    );
    let summary_op = match effectful(&commands).as_slice() {
        [Command::StartModelCall { op, model, request }] => {
            assert_eq!(model.0, "summarizer");
            assert!(request.tools.is_empty());
            assert!(request.blocks.iter().any(|block| {
                block.content.iter().any(|part| {
                    matches!(part, ContentPart::Text(text) if text.contains("old user details"))
                })
            }));
            *op
        }
        other => panic!("expected only summary model call, got {other:#?}"),
    };

    let commands = run_script(
        &mut brain,
        vec![Event::ModelDone {
            op: summary_op,
            output: text_output("summary of old details"),
            usage: usage(),
            est_tokens: 3,
        }],
    );
    assert!(brain.state().log().iter().any(|entry| matches!(
        &entry.record,
        Record::ContextSummary { text, .. } if text == "summary of old details"
    )));
    match effectful(&commands).as_slice() {
        [
            Command::Checkpoint,
            Command::StartModelCall { model, request, .. },
        ] => {
            assert_eq!(model.0, "medium");
            assert!(request.blocks.iter().any(|block| {
                block.role == Role::System
                    && block.content.iter().any(|part| {
                        matches!(part, ContentPart::Text(text) if text.contains("summary of old details"))
                    })
            }));
            assert!(request.blocks.iter().any(|block| {
                block.content.iter().any(
                    |part| matches!(part, ContentPart::Text(text) if text.contains("new question")),
                )
            }));
            assert!(!request.blocks.iter().any(|block| {
                block.content.iter().any(|part| {
                    matches!(part, ContentPart::Text(text) if text.contains("old answer details"))
                })
            }));
        }
        other => panic!("expected checkpoint then main model call, got {other:#?}"),
    }
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
                decision: huggr_core::Decision::Allow,
                est_tokens: 1,
            },
            // A stray DUPLICATE Allow for the now-running op 1: must be ignored,
            // NOT remove the live capability op.
            Event::PermissionDecision {
                op: OpId(1),
                decision: huggr_core::Decision::Allow,
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
/// commands and an identical log.
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
                decision: huggr_core::Decision::Allow,
                est_tokens: 1,
            },
            Event::PermissionDecision {
                op: OpId(1),
                decision: huggr_core::Decision::Allow,
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
/// stringified as the reason. This pins its command sequence.
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
            outcome: huggr_core::OpOutcome::Error(_),
            ..
        }
    )));
}

/// A model error while a **background** op is still running must defer the
/// terminal `Done(Error)` (mirroring `on_model_done`'s deferral): the turn is
/// not over while work is in flight. Once the background op
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
/// commands and identical logs.
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
