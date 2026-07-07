//! Phase 0 exit criterion #1: a scripted "user → model call → tool call →
//! model call → done" session reduces to the expected command sequence.

mod common;

use common::*;
use hugr_core::{
    Brain, Command, ContentPart, ContextBlock, ContextDisposition, ContextPlan, ContextSource,
    DoneReason, Event, LogEntry, ModelSelector, OpId, Record, Role, SamplingParams, Seq, SeqRange,
    StaticPolicy, SummaryCoverage, Timestamp, TokenBudget, ToolSchema, ToolVersioning, TurnPolicy,
    VersionRef,
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
            // result (here: a model-override record) — projection must still
            // group the tool result adjacent to its originating tool call.
            Event::ModelOverride {
                selector: Some(ModelSelector::named("big")),
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "stdout": "a.txt\nb.txt" }),
                version: None,
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
fn versioned_tool_calls_stamp_expected_version_and_route_conflict_retry() {
    let tools = vec![
        ToolSchema::new(
            "fs_read",
            "read",
            json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"]
            }),
        )
        .with_versioning(ToolVersioning::read("path")),
        ToolSchema::new(
            "fs_write",
            "write",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" },
                    "expected_version": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        )
        .with_versioning(ToolVersioning::mutation("path", "expected_version")),
    ];
    let mut brain = Brain::new(Box::new(StaticPolicy::default().with_tools(tools)));

    let commands = run_script(
        &mut brain,
        vec![
            user("edit src/lib.rs"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("read-1", "fs_read", json!({ "path": "src/lib.rs" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityDone {
                op: OpId(1),
                result: json!({ "path": "src/lib.rs", "content": "old", "version": "v1" }),
                version: Some(VersionRef::new("src/lib.rs", "v1")),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: tool_output(
                    "write-1",
                    "fs_write",
                    json!({ "path": "src/lib.rs", "content": "new" }),
                ),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityError {
                op: OpId(3),
                error: json!({
                    "error": "conflict",
                    "path": "src/lib.rs",
                    "current_version": "v2",
                    "current_content": "changed"
                }),
                conflict: Some(VersionRef::new("src/lib.rs", "v2")),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(4),
                output: tool_output("read-2", "fs_read", json!({ "path": "src/lib.rs" })),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    let write_args = commands
        .iter()
        .find_map(|cmd| match cmd {
            Command::StartCapability {
                op: OpId(3),
                name,
                args,
            } if name == "fs_write" => Some(args),
            _ => None,
        })
        .expect("write capability should start");
    assert_eq!(write_args["expected_version"], json!("v1"));

    assert_eq!(
        brain.state().versions().get("src/lib.rs"),
        Some(&"v2".to_string())
    );
    let restored = Brain::from_log(
        Box::new(StaticPolicy::default()),
        brain.state().log().to_vec(),
    );
    assert_eq!(
        restored.state().versions().get("src/lib.rs"),
        Some(&"v2".to_string())
    );

    let retry_read = commands.iter().any(|cmd| {
        matches!(
            cmd,
            Command::StartCapability {
                op: OpId(5),
                name,
                ..
            } if name == "fs_read"
        )
    });
    assert!(
        retry_read,
        "conflict should be routed back so the model can re-read"
    );
}

#[test]
fn model_override_forces_one_turn_then_clears() {
    let script = vec![
        Event::ModelOverride {
            selector: Some(ModelSelector::named("big")),
        },
        user("ordinary request"),
        Event::ModelDone {
            op: OpId(0),
            output: text_output("Done."),
            usage: usage(),
            est_tokens: 1,
        },
        user("another ordinary request"),
    ];
    let mut brain = Brain::new(Box::new(StaticPolicy::default()));
    let commands = run_script(&mut brain, script);
    let selectors: Vec<_> = commands
        .iter()
        .filter_map(|command| match command {
            Command::StartModelCall { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        selectors,
        vec![ModelSelector::named("big"), ModelSelector::named("medium")]
    );
    assert!(brain.state().next_model_override().is_none());
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

/// A2: durable summaries round-trip through the log and later projections evict
/// the exact covered source span to references rather than deleting it.
#[test]
fn summary_records_round_trip_and_evict_covered_span_to_refs() {
    let log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "first long turn".to_string(),
                est_tokens: 40,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: text_output("first long answer"),
                est_tokens: 60,
            },
        ),
        LogEntry::new(
            Seq(2),
            Timestamp(0),
            Record::Summary {
                op: OpId(1),
                text: "The first turn established the task.".to_string(),
                summary_of: SeqRange::new(Seq(0), Seq(1)),
                coverage: SummaryCoverage::Complete,
                tier: ModelSelector::named("small"),
                est_tokens_in: 100,
                est_tokens_out: 8,
            },
        ),
    ];

    let json = serde_json::to_string(&log).expect("summary log serializes");
    let restored: Vec<LogEntry> = serde_json::from_str(&json).expect("summary log deserializes");
    assert_eq!(restored, log);

    let policy = StaticPolicy::default();
    let plan = policy.project_context(&restored, TokenBudget::new(128));
    let request = plan.to_model_request();

    assert_eq!(plan.entries.len(), 3);
    assert_eq!(request.blocks.len(), 3);
    assert_eq!(plan.totals.referenced_tokens, 2);
    assert_eq!(plan.totals.summarized_tokens, 8);

    assert!(matches!(
        plan.entries[0].disposition,
        ContextDisposition::Referenced { .. }
    ));
    assert!(matches!(
        plan.entries[1].disposition,
        ContextDisposition::Referenced { .. }
    ));
    assert!(matches!(
        plan.entries[2].disposition,
        ContextDisposition::Summarized { .. }
    ));

    assert!(matches!(
        &request.blocks[0].content[0],
        ContentPart::Ref {
            reference,
            summary,
            est_tokens: 1,
        } if reference == "log:0" && summary == "covered by summary log:2"
    ));
}

/// A3: crossing the high-water mark starts a small-tier compaction model call;
/// its recorded result becomes a summary, then the real turn is re-projected.
#[test]
fn automatic_compaction_summarizes_then_reprojects_and_replays() {
    let prior_log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "old user turn with many details".to_string(),
                est_tokens: 10,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: text_output("old assistant answer with many details"),
                est_tokens: 10,
            },
        ),
    ];
    let script = vec![
        Event::UserInput {
            content: json!("new request"),
            mode: hugr_core::SteerMode::Queue,
            est_tokens: 1,
        },
        Event::ModelDone {
            op: OpId(1),
            output: text_output("Old turn summary."),
            usage: usage(),
            est_tokens: 3,
        },
        Event::ModelDone {
            op: OpId(2),
            output: text_output("Final answer."),
            usage: usage(),
            est_tokens: 1,
        },
    ];

    let policy = || {
        StaticPolicy::default()
            .with_context_budget(TokenBudget::new(20))
            .with_compaction_high_water_percent(90)
    };
    let mut first = Brain::from_log(Box::new(policy()), prior_log.clone());
    let commands_first = run_script(&mut first, script.clone());
    let effectful_first = effectful(&commands_first);

    assert!(
        matches!(
            effectful_first.as_slice(),
            [
                Command::StartModelCall {
                    op: OpId(1),
                    model,
                    request
                },
                Command::Checkpoint,
                Command::StartModelCall {
                    op: OpId(2),
                    model: turn_model,
                    request: turn_request
                },
                Command::Checkpoint,
                Command::Done {
                    reason: DoneReason::EndTurn
                },
            ] if *model == ModelSelector::named("medium")
                && request.tools.is_empty()
                && request.extra["kind"] == "compaction"
                && *turn_model == ModelSelector::named("medium")
                && turn_request.blocks.iter().any(|block| block.content.iter().any(|part| matches!(part, ContentPart::Ref { reference, .. } if reference == "log:0")))
                && turn_request.blocks.iter().any(|block| block.content.iter().any(|part| matches!(part, ContentPart::Text(text) if text.contains("Old turn summary."))))
        ),
        "unexpected command sequence: {effectful_first:#?}"
    );

    let summary = first
        .state()
        .log()
        .iter()
        .find_map(|entry| match &entry.record {
            Record::Summary {
                summary_of,
                tier,
                est_tokens_in,
                est_tokens_out,
                text,
                ..
            } => Some((summary_of, tier, est_tokens_in, est_tokens_out, text)),
            _ => None,
        })
        .expect("summary record is appended");
    assert_eq!(*summary.0, SeqRange::new(Seq(0), Seq(1)));
    // Compaction is now routed through `TurnPolicy::choose_model` with the
    // `Compaction` phase (no longer hardcoded to `small`). `StaticPolicy` has a
    // single tier, so it falls back to its default model (`medium`) rather than
    // requiring a registered `small` model.
    assert_eq!(*summary.1, ModelSelector::named("medium"));
    assert_eq!(*summary.2, 20);
    assert_eq!(*summary.3, 3);
    assert_eq!(summary.4, "Old turn summary.");

    let mut second = Brain::from_log(Box::new(policy()), prior_log);
    let commands_second = run_script(&mut second, script);
    assert_eq!(commands_first, commands_second);
    assert_eq!(first.state().log(), second.state().log());
}

/// A4: a host-injected manual compaction event starts exactly one small-tier
/// compaction pass and returns to idle after checkpointing.
#[test]
fn manual_compaction_event_runs_one_pass_without_starting_turn() {
    let prior_log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "old user turn with many details".to_string(),
                est_tokens: 10,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: text_output("old assistant answer with many details"),
                est_tokens: 10,
            },
        ),
    ];
    let script = vec![
        Event::CompactContext,
        Event::ModelDone {
            op: OpId(1),
            output: text_output("Manual summary."),
            usage: usage(),
            est_tokens: 3,
        },
    ];

    let policy = || {
        StaticPolicy::default()
            .with_context_budget(TokenBudget::new(20))
            .with_compaction_high_water_percent(0)
    };
    let mut first = Brain::from_log(Box::new(policy()), prior_log.clone());
    let commands_first = run_script(&mut first, script.clone());
    let effectful_first = effectful(&commands_first);

    assert!(
        matches!(
            effectful_first.as_slice(),
            [
                Command::StartModelCall {
                    op: OpId(1),
                    model,
                    request
                },
                Command::Checkpoint,
            ] if *model == ModelSelector::named("medium")
                && request.extra["kind"] == "compaction"
                && request.extra["summary_of"]["start"] == 0
                && request.extra["summary_of"]["end"] == 0
        ),
        "unexpected command sequence: {effectful_first:#?}"
    );
    assert_eq!(first.state().inflight_len(), 0);

    let summary = first
        .state()
        .log()
        .iter()
        .find_map(|entry| match &entry.record {
            Record::Summary {
                summary_of,
                tier,
                est_tokens_in,
                est_tokens_out,
                text,
                ..
            } => Some((summary_of, tier, est_tokens_in, est_tokens_out, text)),
            _ => None,
        })
        .expect("summary record is appended");
    assert_eq!(*summary.0, SeqRange::new(Seq(0), Seq(0)));
    // Routed through `choose_model` (Compaction phase); `StaticPolicy` falls
    // back to its default `medium` model instead of a hardcoded `small` tier.
    assert_eq!(*summary.1, ModelSelector::named("medium"));
    assert_eq!(*summary.2, 10);
    assert_eq!(*summary.3, 3);
    assert_eq!(summary.4, "Manual summary.");

    let mut second = Brain::from_log(Box::new(policy()), prior_log);
    let commands_second = run_script(&mut second, script);
    assert_eq!(commands_first, commands_second);
    assert_eq!(first.state().log(), second.state().log());
}

/// A **duplicate** permission `Allow` for an op that already started must be a
/// no-op — it must never drop the live op from the in-flight table
/// (ARCHITECTURE §4.1). The reducer peeks for `AwaitingPermission` before
/// removing; a stray decision for a non-awaiting op leaves state untouched.
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
                version: None,
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
                version: None,
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
                version: None,
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

/// A prior log heavy enough to cross a small high-water budget: one user turn
/// and one assistant answer (the shape reused by the A3/A4 compaction tests).
fn compaction_prior_log() -> Vec<LogEntry> {
    vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "old user turn with many details".to_string(),
                est_tokens: 10,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: text_output("old assistant answer with many details"),
                est_tokens: 10,
            },
        ),
    ]
}

/// New user turn (crosses the high-water mark → compaction), then the summary
/// model result, then the real turn's final answer.
fn compaction_script() -> Vec<Event> {
    vec![
        Event::UserInput {
            content: json!("new request"),
            mode: hugr_core::SteerMode::Queue,
            est_tokens: 1,
        },
        Event::ModelDone {
            op: OpId(1),
            output: text_output("Old turn summary."),
            usage: usage(),
            est_tokens: 3,
        },
        Event::ModelDone {
            op: OpId(2),
            output: text_output("Final answer."),
            usage: usage(),
            est_tokens: 1,
        },
    ]
}

/// Tool_result ids in a request that have no preceding tool_use id — an invalid
/// provider message sequence (the 400 auto-compaction must never cause).
fn unmatched_tool_results(request: &hugr_core::ModelRequest) -> Vec<String> {
    let mut tool_use_ids = std::collections::HashSet::new();
    let mut unmatched = Vec::new();
    for block in &request.blocks {
        for part in &block.content {
            match part {
                ContentPart::ToolUse { id, .. } => {
                    tool_use_ids.insert(id.clone());
                }
                ContentPart::ToolResult { id, .. } => {
                    if !tool_use_ids.contains(id) {
                        unmatched.push(id.clone());
                    }
                }
                _ => {}
            }
        }
    }
    unmatched
}

/// Fix 1: the compaction span must never end between a `ModelOutput` carrying a
/// tool call and the `ToolResult` that answers it. The naive token boundary here
/// lands on seq 1 (the tool_use); the selector must extend the span to seq 2 so
/// the follow-up projection stays a valid provider message sequence.
#[test]
fn compaction_span_never_splits_a_tool_use_result_group() {
    let log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "please run ls".to_string(),
                est_tokens: 10,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "ls" })),
                est_tokens: 10,
            },
        ),
        LogEntry::new(
            Seq(2),
            Timestamp(0),
            Record::ToolResult {
                op: OpId(0),
                name: "shell".to_string(),
                call_id: "call-1".to_string(),
                result: json!({ "ok": true }),
                version: None,
                est_tokens: 10,
            },
        ),
        LogEntry::new(
            Seq(3),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(1),
                output: text_output("done"),
                est_tokens: 10,
            },
        ),
    ];
    let policy = StaticPolicy::default();
    let plan = policy.project_context(&log, TokenBudget::new(20));
    let target = policy
        .select_compaction_span(&log, &plan)
        .expect("a compaction span is selected");
    // Span ends on the tool_result (seq 2), not mid-group on the tool_use (seq 1),
    // and its token estimate includes the pulled-in tool_result.
    assert_eq!(target.summary_of, SeqRange::new(Seq(0), Seq(2)));
    assert_eq!(target.est_tokens_in, 30);

    // Projecting with a summary that covers the whole group yields no orphan.
    let summary = |range: SeqRange| {
        LogEntry::new(
            Seq(4),
            Timestamp(0),
            Record::Summary {
                op: OpId(2),
                text: "summary".to_string(),
                summary_of: range,
                coverage: SummaryCoverage::Complete,
                tier: ModelSelector::named("medium"),
                est_tokens_in: 30,
                est_tokens_out: 3,
            },
        )
    };
    let mut whole = log.clone();
    whole.push(summary(target.summary_of));
    let request = policy
        .project_context(&whole, TokenBudget::new(1000))
        .to_model_request();
    assert!(
        unmatched_tool_results(&request).is_empty(),
        "orphaned tool_result in projection: {request:#?}"
    );

    // Sanity: had the span stopped mid-group at seq 1, the tool_result at seq 2
    // would be projected orphaned — exactly the provider 400 the fix prevents.
    let mut split = log.clone();
    split.push(summary(SeqRange::new(Seq(0), Seq(1))));
    let split_request = policy
        .project_context(&split, TokenBudget::new(1000))
        .to_model_request();
    assert_eq!(
        unmatched_tool_results(&split_request),
        vec!["call-1".to_string()]
    );
}

/// Fix 3 (default): the default `compaction_request` is byte-identical to the
/// prompt/rendering the reducer produced before the hook existed.
#[test]
fn default_compaction_request_is_byte_identical() {
    let policy = StaticPolicy::default();
    let log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            Record::UserMessage {
                text: "hello".to_string(),
                est_tokens: 1,
                steer: hugr_core::SteerMode::Queue,
            },
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::ModelOutput {
                op: OpId(0),
                output: text_output("did stuff"),
                est_tokens: 1,
            },
        ),
        LogEntry::new(
            Seq(2),
            Timestamp(0),
            Record::ToolResult {
                op: OpId(0),
                name: "shell".to_string(),
                call_id: "call-1".to_string(),
                result: json!({ "ok": true }),
                version: None,
                est_tokens: 1,
            },
        ),
    ];

    let request = policy.compaction_request(&log, SeqRange::new(Seq(0), Seq(2)));

    assert_eq!(
        request.blocks[0],
        ContextBlock::new(
            Role::System,
            vec![ContentPart::Text(
                "Summarize the provided Hugr log span for future context. Preserve user intent, decisions, tool results, and unresolved work. Return concise plain text only."
                    .to_string()
            )],
        )
    );
    assert_eq!(
        request.blocks[1],
        ContextBlock::new(
            Role::User,
            vec![ContentPart::Text(
                "log:0 user: hello\nlog:1 assistant: did stuff\nlog:2 tool shell: {\"ok\":true}"
                    .to_string()
            )],
        )
    );
    assert!(request.tools.is_empty());
    assert_eq!(request.params, SamplingParams::default());
    assert_eq!(request.extra["kind"], "compaction");
    assert_eq!(request.extra["summary_of"]["start"], 0);
    assert_eq!(request.extra["summary_of"]["end"], 2);
}

/// Fix 3 (override): a custom policy can change the summarization rendering, and
/// the reducer uses it — the emitted compaction request carries the custom text.
#[test]
fn compaction_prompt_and_rendering_are_overridable() {
    struct CustomSummaryPolicy {
        base: StaticPolicy,
    }

    impl TurnPolicy for CustomSummaryPolicy {
        fn choose_model(&self, state: &hugr_core::BrainState) -> ModelSelector {
            self.base.choose_model(state)
        }

        fn context_budget(&self, state: &hugr_core::BrainState) -> TokenBudget {
            self.base.context_budget(state)
        }

        fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan {
            self.base.project_context(log, budget)
        }

        fn compaction_high_water(
            &self,
            state: &hugr_core::BrainState,
            budget: TokenBudget,
        ) -> Option<u64> {
            self.base.compaction_high_water(state, budget)
        }

        fn needs_permission(&self, capability: &str) -> bool {
            self.base.needs_permission(capability)
        }

        fn render_summary_record(&self, seq: Seq, record: &Record) -> Option<String> {
            self.base
                .render_summary_record(seq, record)
                .map(|line| format!("CUSTOM[{}]: {line}", seq.0))
        }
    }

    let policy = CustomSummaryPolicy {
        base: StaticPolicy::default()
            .with_context_budget(TokenBudget::new(20))
            .with_compaction_high_water_percent(90),
    };
    let mut brain = Brain::from_log(Box::new(policy), compaction_prior_log());
    let commands = run_script(&mut brain, compaction_script());

    let request = commands
        .iter()
        .find_map(|command| match command {
            Command::StartModelCall { request, .. } if request.extra["kind"] == "compaction" => {
                Some(request)
            }
            _ => None,
        })
        .expect("a compaction model call is emitted");

    let text_of = |role: Role| {
        request
            .blocks
            .iter()
            .find(|block| block.role == role)
            .and_then(|block| {
                block.content.iter().find_map(|part| match part {
                    ContentPart::Text(text) => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default()
    };

    // The overridden rendering is used for the span.
    assert!(
        text_of(Role::User).contains("CUSTOM[0]:"),
        "custom rendering not used: {}",
        text_of(Role::User)
    );
    // The default (non-overridden) English prompt still leads the request,
    // proving the reducer calls the default compaction_request which delegates
    // to the overridden render_summary_record.
    assert!(
        text_of(Role::System).starts_with("Summarize the provided Hugr log span"),
        "default system prompt missing: {}",
        text_of(Role::System)
    );
}

/// `Event::ModelOverride` is durable: it appends a `Record::ModelOverride`
/// (plus a `Checkpoint`), and `Brain::from_log` re-derives the pending override
/// from the log — the last override record not yet consumed by a subsequent
/// main-turn `ModelOutput` (the log is the source of truth, ARCHITECTURE §3.1).
#[test]
fn model_override_is_logged_and_survives_resume() {
    let mut brain = Brain::new(Box::new(StaticPolicy::default()));
    let commands = run_script(
        &mut brain,
        vec![Event::ModelOverride {
            selector: Some(ModelSelector::named("big")),
        }],
    );
    // Pinned command sequence: the durable record is checkpointed.
    assert_eq!(effectful(&commands), vec![&Command::Checkpoint]);
    assert!(matches!(
        brain.state().log().last().map(|entry| &entry.record),
        Some(Record::ModelOverride { selector: Some(s) }) if *s == ModelSelector::named("big")
    ));

    // Crash/resume before the override is consumed: the fold restores it.
    let resumed = Brain::from_log(
        Box::new(StaticPolicy::default()),
        brain.state().log().to_vec(),
    );
    assert_eq!(
        resumed.state().next_model_override(),
        Some(&ModelSelector::named("big")),
        "a pending override must survive checkpoint/resume via the log"
    );

    // Consume it with a real turn: the next main-turn ModelOutput record marks
    // the override consumed, so a later resume restores nothing.
    run_script(
        &mut brain,
        vec![
            user("ordinary request"),
            Event::ModelDone {
                op: OpId(0),
                output: text_output("Done."),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );
    assert!(brain.state().next_model_override().is_none());
    let resumed = Brain::from_log(
        Box::new(StaticPolicy::default()),
        brain.state().log().to_vec(),
    );
    assert!(
        resumed.state().next_model_override().is_none(),
        "a consumed override must not be resurrected by the fold"
    );

    // Clearing (`selector: None`) is durable too: it supersedes a pending one.
    let mut cleared = Brain::new(Box::new(StaticPolicy::default()));
    run_script(
        &mut cleared,
        vec![
            Event::ModelOverride {
                selector: Some(ModelSelector::named("big")),
            },
            Event::ModelOverride { selector: None },
        ],
    );
    let resumed = Brain::from_log(
        Box::new(StaticPolicy::default()),
        cleared.state().log().to_vec(),
    );
    assert!(resumed.state().next_model_override().is_none());

    // Determinism: the whole override path replays bit-for-bit.
    assert_deterministic_replay(
        || Brain::new(Box::new(StaticPolicy::default())),
        || {
            vec![
                Event::ModelOverride {
                    selector: Some(ModelSelector::named("big")),
                },
                user("ordinary request"),
                Event::ModelDone {
                    op: OpId(0),
                    output: text_output("Done."),
                    usage: usage(),
                    est_tokens: 1,
                },
            ]
        },
    );
}

/// A compaction pass does not consume a pending override: compaction logs a
/// `Summary` record (never a `ModelOutput`), so the fold keeps the override
/// pending for the real turn that resumes afterwards.
#[test]
fn model_override_survives_a_compaction_pass_in_the_fold() {
    let log = vec![
        LogEntry::new(
            Seq(0),
            Timestamp(0),
            serde_json::from_value(json!({
                "ModelOverride": { "selector": { "Named": "big" } }
            }))
            .unwrap(),
        ),
        LogEntry::new(
            Seq(1),
            Timestamp(0),
            Record::Summary {
                op: OpId(0),
                text: "old span summary".to_string(),
                summary_of: SeqRange::new(Seq(0), Seq(0)),
                coverage: SummaryCoverage::Complete,
                tier: ModelSelector::named("small"),
                est_tokens_in: 10,
                est_tokens_out: 2,
            },
        ),
    ];
    let brain = Brain::from_log(Box::new(StaticPolicy::default()), log);
    assert_eq!(
        brain.state().next_model_override(),
        Some(&ModelSelector::named("big")),
        "a compaction summary must not consume the pending override"
    );
}

/// Queue vs interrupt input write distinguishable `UserMessage` records: the
/// fold is no longer lossy about how input steered the turn (log fidelity).
#[test]
fn user_message_records_carry_their_steer_mode() {
    let mut brain = Brain::with_default_policy();
    run_script(
        &mut brain,
        vec![
            user("first"),          // idle → Queue
            user_interrupt("stop"), // mid-turn interrupt
            Event::OpCancelled { op: OpId(0) },
        ],
    );
    let steers: Vec<_> = brain
        .state()
        .log()
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::UserMessage { steer, .. } => Some(*steer),
            _ => None,
        })
        .collect();
    assert_eq!(
        steers,
        vec![hugr_core::SteerMode::Queue, hugr_core::SteerMode::Interrupt]
    );
}

/// Serde back-compat: an old trace's `UserMessage` JSON (no `steer` key) still
/// loads, defaulting to `Queue`.
#[test]
fn old_user_message_json_without_steer_defaults_to_queue() {
    let record: Record = serde_json::from_value(json!({
        "UserMessage": { "text": "hi", "est_tokens": 3 }
    }))
    .unwrap();
    assert!(matches!(
        record,
        Record::UserMessage {
            steer: hugr_core::SteerMode::Queue,
            ..
        }
    ));
}
