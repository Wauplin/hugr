//! Phase 0 exit criterion #1: a scripted "user → model call → tool call →
//! model call → done" session reduces to the expected command sequence.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use hugr_core::{
    Brain, Command, ContentPart, ContextDisposition, ContextPlan, ContextSource, DoneReason, Event,
    LogEntry, ModelSelector, OpId, Record, RoutingInputs, RoutingPhase, RoutingPolicy, Seq,
    SeqRange, SkillDescriptor, StaticPolicy, SummaryCoverage, Timestamp, TokenBudget, ToolRisk,
    ToolSchema, ToolVersioning, TurnPolicy, VersionRef,
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
fn accepted_plan_persists_and_projects_into_future_context() {
    let mut brain = Brain::with_default_policy();
    let commands = run_script(
        &mut brain,
        vec![
            Event::PlanAccepted {
                text: "1. Inspect failing test\n2. Patch parser\n3. Run cargo test".to_string(),
                est_tokens: 12,
            },
            user("continue"),
        ],
    );

    assert!(brain.state().log().iter().any(|entry| {
        matches!(
            &entry.record,
            Record::Plan { text, est_tokens } if text.contains("Patch parser") && *est_tokens == 12
        )
    }));

    let request = commands
        .iter()
        .find_map(|cmd| match cmd {
            Command::StartModelCall { request, .. } => Some(request),
            _ => None,
        })
        .expect("model request");
    let rendered = request
        .blocks
        .iter()
        .flat_map(|block| &block.content)
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Accepted task plan"));
    assert!(rendered.contains("Patch parser"));
}

#[test]
fn todo_state_persists_and_projects_latest_progress() {
    let mut brain = Brain::with_default_policy();
    let commands = run_script(
        &mut brain,
        vec![
            Event::TodoUpdated {
                items: vec![
                    hugr_core::TodoItem::new("inspect"),
                    hugr_core::TodoItem::new("test"),
                ],
                est_tokens: 4,
            },
            Event::TodoUpdated {
                items: vec![
                    hugr_core::TodoItem::done("inspect"),
                    hugr_core::TodoItem::new("test"),
                ],
                est_tokens: 4,
            },
            user("status"),
        ],
    );

    let todo_records = brain
        .state()
        .log()
        .iter()
        .filter(|entry| matches!(entry.record, Record::TodoList { .. }))
        .count();
    assert_eq!(todo_records, 2);

    let request = commands
        .iter()
        .find_map(|cmd| match cmd {
            Command::StartModelCall { request, .. } => Some(request),
            _ => None,
        })
        .expect("model request");
    let rendered = request
        .blocks
        .iter()
        .flat_map(|block| &block.content)
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("Durable todo progress"));
    assert!(rendered.contains("1/2 done"));
    assert!(rendered.contains("[x] inspect"));
}

#[test]
fn skill_invocation_records_activation_and_projects_instructions() {
    let skill = SkillDescriptor::new(
        "rust-reviewer",
        "Rust Reviewer",
        "Check Rust changes for ownership, error handling, and missing tests.",
    )
    .with_summary("Review Rust diffs.")
    .with_est_tokens(12);
    let mut brain = Brain::new(Box::new(
        StaticPolicy::default().with_skills([skill.clone()]),
    ));

    let commands = run_script(
        &mut brain,
        vec![
            user("review this change"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", &skill.tool_name(), json!({})),
                usage: usage(),
                est_tokens: 1,
            },
            Event::ModelDone {
                op: OpId(2),
                output: text_output("Review complete."),
                usage: usage(),
                est_tokens: 1,
            },
        ],
    );

    let first_request = commands
        .iter()
        .find_map(|cmd| match cmd {
            Command::StartModelCall {
                op: OpId(0),
                request,
                ..
            } => Some(request),
            _ => None,
        })
        .expect("initial model call");
    assert_eq!(first_request.tools[0].name, "skill__rust-reviewer");

    let followup_request = commands
        .iter()
        .find_map(|cmd| match cmd {
            Command::StartModelCall {
                op: OpId(2),
                request,
                ..
            } => Some(request),
            _ => None,
        })
        .expect("follow-up model call after skill activation");
    assert!(followup_request.blocks.iter().any(|block| {
        block.content.iter().any(|part| {
            matches!(
                part,
                ContentPart::Text(text)
                    if text.contains("Active skill `rust-reviewer`")
                        && text.contains("Check Rust changes")
            )
        })
    }));

    assert!(brain.state().log().iter().any(|entry| {
        matches!(
            &entry.record,
            Record::SkillActivated { id, title, est_tokens, .. }
                if id == "rust-reviewer" && title == "Rust Reviewer" && *est_tokens == 12
        )
    }));
}

#[test]
fn routing_inputs_are_purely_derived_for_turn_and_followup() {
    struct CapturingPolicy {
        base: StaticPolicy,
        seen: Arc<Mutex<Vec<RoutingInputs>>>,
    }

    impl TurnPolicy for CapturingPolicy {
        fn choose_model(
            &self,
            state: &hugr_core::BrainState,
            inputs: &RoutingInputs,
        ) -> ModelSelector {
            self.seen.lock().unwrap().push(inputs.clone());
            self.base.choose_model(state, inputs)
        }

        fn context_budget(&self, state: &hugr_core::BrainState) -> TokenBudget {
            self.base.context_budget(state)
        }

        fn project_context(&self, log: &[LogEntry], budget: TokenBudget) -> ContextPlan {
            self.base.project_context(log, budget)
        }

        fn needs_permission(&self, capability: &str) -> bool {
            self.base.needs_permission(capability)
        }
    }

    let seen = Arc::new(Mutex::new(Vec::new()));
    let policy = CapturingPolicy {
        base: StaticPolicy::default().with_context_budget(TokenBudget::new(4)),
        seen: seen.clone(),
    };
    let mut brain = Brain::new(Box::new(policy));

    run_script(
        &mut brain,
        vec![
            user("first"),
            Event::ModelDone {
                op: OpId(0),
                output: tool_output("call-1", "shell", json!({ "cmd": "false" })),
                usage: usage(),
                est_tokens: 1,
            },
            Event::CapabilityError {
                op: OpId(1),
                error: json!({ "error": "test failed" }),
                conflict: None,
                est_tokens: 1,
            },
        ],
    );

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].phase, RoutingPhase::Normal);
    assert_eq!(seen[0].tool_risk, ToolRisk::None);
    assert_eq!(seen[0].recent_failures, 0);
    assert!(seen[0].context_pressure > 0.0);
    assert_eq!(seen[1].phase, RoutingPhase::ToolFollowup);
    assert_eq!(seen[1].tool_risk, ToolRisk::Failed);
    assert!(seen[1].recent_failures >= 1);
    assert!(seen[1].context_pressure >= seen[0].context_pressure);
}

#[test]
fn routing_policy_deterministically_uses_small_medium_and_big() {
    let mut small_brain = Brain::new(Box::new(RoutingPolicy::default()));
    let small_commands = run_script(
        &mut small_brain,
        vec![user("classify this request as safe or unsafe")],
    );
    assert!(matches!(
        first_model_selector(&small_commands),
        Some(ModelSelector::Named(name)) if name == "small"
    ));

    let mut medium_brain = Brain::new(Box::new(RoutingPolicy::default()));
    let medium_commands = run_script(&mut medium_brain, vec![user("write a short note")]);
    assert!(matches!(
        first_model_selector(&medium_commands),
        Some(ModelSelector::Named(name)) if name == "medium"
    ));

    let failure_script = vec![
        user("fix the failing tests"),
        Event::ModelDone {
            op: OpId(0),
            output: tool_output("call-1", "shell", json!({ "cmd": "cargo test" })),
            usage: usage(),
            est_tokens: 1,
        },
        Event::CapabilityError {
            op: OpId(1),
            error: json!({ "error": "test failed", "stderr": "FAILED test_one" }),
            conflict: None,
            est_tokens: 1,
        },
        Event::ModelDone {
            op: OpId(2),
            output: text_output("I see the failing test."),
            usage: usage(),
            est_tokens: 1,
        },
    ];
    let mut big_brain = Brain::new(Box::new(RoutingPolicy::default()));
    let big_commands = run_script(&mut big_brain, failure_script.clone());
    assert!(matches!(
        last_model_selector(&big_commands),
        Some(ModelSelector::Named(name)) if name == "big"
    ));
    let big_routing = big_brain
        .state()
        .log()
        .iter()
        .find_map(|entry| match &entry.record {
            Record::OpEnded {
                op: OpId(2), meta, ..
            } => meta.routing.as_ref(),
            _ => None,
        })
        .expect("big model op records routing metadata");
    assert_eq!(big_routing.selector, ModelSelector::named("big"));
    assert!(
        big_routing
            .reasons
            .iter()
            .any(|reason| reason.contains("recent failure count")),
        "routing reasons: {:?}",
        big_routing.reasons
    );
    assert!(
        big_routing.inputs["recent_failures"].as_u64().unwrap_or(0) >= 2,
        "routing inputs: {}",
        big_routing.inputs
    );

    let mut replay_brain = Brain::new(Box::new(RoutingPolicy::default()));
    let replay_commands = run_script(&mut replay_brain, failure_script);
    assert_eq!(big_commands, replay_commands);
    assert_eq!(big_brain.state().log(), replay_brain.state().log());
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
    let mut brain = Brain::new(Box::new(RoutingPolicy::default()));
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
        LogEntry {
            seq: Seq(0),
            at: Timestamp(0),
            record: Record::UserMessage {
                text: "first long turn".to_string(),
                est_tokens: 40,
            },
        },
        LogEntry {
            seq: Seq(1),
            at: Timestamp(0),
            record: Record::ModelOutput {
                op: OpId(0),
                output: text_output("first long answer"),
                est_tokens: 60,
            },
        },
        LogEntry {
            seq: Seq(2),
            at: Timestamp(0),
            record: Record::Summary {
                op: OpId(1),
                text: "The first turn established the task.".to_string(),
                summary_of: SeqRange::new(Seq(0), Seq(1)),
                coverage: SummaryCoverage::Complete,
                tier: ModelSelector::named("small"),
                est_tokens_in: 100,
                est_tokens_out: 8,
            },
        },
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
        LogEntry {
            seq: Seq(0),
            at: Timestamp(0),
            record: Record::UserMessage {
                text: "old user turn with many details".to_string(),
                est_tokens: 10,
            },
        },
        LogEntry {
            seq: Seq(1),
            at: Timestamp(0),
            record: Record::ModelOutput {
                op: OpId(0),
                output: text_output("old assistant answer with many details"),
                est_tokens: 10,
            },
        },
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
            ] if *model == ModelSelector::named("small")
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
    assert_eq!(*summary.1, ModelSelector::named("small"));
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
        LogEntry {
            seq: Seq(0),
            at: Timestamp(0),
            record: Record::UserMessage {
                text: "old user turn with many details".to_string(),
                est_tokens: 10,
            },
        },
        LogEntry {
            seq: Seq(1),
            at: Timestamp(0),
            record: Record::ModelOutput {
                op: OpId(0),
                output: text_output("old assistant answer with many details"),
                est_tokens: 10,
            },
        },
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
            ] if *model == ModelSelector::named("small")
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
    assert_eq!(*summary.1, ModelSelector::named("small"));
    assert_eq!(*summary.2, 10);
    assert_eq!(*summary.3, 3);
    assert_eq!(summary.4, "Manual summary.");

    let mut second = Brain::from_log(Box::new(policy()), prior_log);
    let commands_second = run_script(&mut second, script);
    assert_eq!(commands_first, commands_second);
    assert_eq!(first.state().log(), second.state().log());
}

fn first_model_selector(commands: &[Command]) -> Option<&ModelSelector> {
    commands.iter().find_map(|command| match command {
        Command::StartModelCall { model, .. } => Some(model),
        _ => None,
    })
}

fn last_model_selector(commands: &[Command]) -> Option<&ModelSelector> {
    commands.iter().rev().find_map(|command| match command {
        Command::StartModelCall { model, .. } => Some(model),
        _ => None,
    })
}
