//! The `Command::AskUser` ↔ [`Event::UserAnswer`] round-trip.
//!
//! `Command::AskUser` documents its reply channel: the host answers with
//! [`Event::UserAnswer`] (the native engine prompts the user; the sub-agent
//! runner auto-answers with an error). The brain-side contract pinned here is
//! the reply half of that round-trip: a `UserAnswer` resolves the pending op as
//! **one** consolidated tool-result-shaped [`Record::ToolResult`] and resumes
//! the turn, exactly like a capability result (brain.rs `on_user_answer`,
//! ARCHITECTURE §4.5). In the scripted session the model triggers the question
//! as an `ask_user` tool call; the host answers the resulting op with
//! `Event::UserAnswer` instead of `CapabilityDone`.

mod common;

use common::*;
use hugr_core::{Brain, Command, ContentPart, DoneReason, Event, OpId, Record, Role};
use serde_json::json;

/// The event script: user → model asks the user (tool call) → the host relays
/// the user's answer via `Event::UserAnswer` → resumed model turn → done.
fn ask_user_script() -> Vec<Event> {
    vec![
        // 1. User kicks off a turn → brain starts a model call (op 0).
        user("deploy the service"),
        // 2. The model wants to ask the user a question (op 0 result) → brain
        //    starts the `ask_user` op (op 1). The host performs it by prompting
        //    the user (a free-form question, not a permission decision).
        Event::ModelDone {
            op: OpId(0),
            output: tool_output(
                "call-ask",
                "ask_user",
                json!({ "message": "Which environment: staging or production?" }),
            ),
            usage: usage(),
            est_tokens: 1,
        },
        // 3. The host answers the pending op with the user's reply — the
        //    `Event::UserAnswer` half of the AskUser round-trip.
        Event::UserAnswer {
            op: OpId(1),
            answer: json!({ "answer": "staging" }),
            est_tokens: 1,
        },
        // 4. The resumed model turn consumes the answer and finishes.
        Event::ModelDone {
            op: OpId(2),
            output: text_output("Deploying to staging."),
            usage: usage(),
            est_tokens: 1,
        },
    ]
}

/// The canonical AskUser turn reduces to the expected effectful command
/// sequence, and the answer folds into the durable log as a tool-result-shaped
/// record that the resumed turn's projection consumes.
#[test]
fn user_answer_folds_as_tool_result_and_resumes_the_turn() {
    let mut brain = Brain::with_default_policy();
    let commands = run_script(&mut brain, ask_user_script());

    // The effectful sequence: the answer resumes the turn (op 2) exactly like a
    // capability result would.
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
            ] if name == "ask_user"
        ),
        "unexpected command sequence: {effectful:#?}"
    );

    // The answer is durable as *one* consolidated `ToolResult` record carrying
    // the opaque answer verbatim and correlating to the originating tool call.
    let tool_result = brain
        .state()
        .log()
        .iter()
        .find_map(|entry| match &entry.record {
            Record::ToolResult {
                op,
                name,
                call_id,
                result,
                est_tokens,
                ..
            } => Some((
                *op,
                name.clone(),
                call_id.clone(),
                result.clone(),
                *est_tokens,
            )),
            _ => None,
        })
        .expect("the user's answer must fold as a ToolResult record");
    assert_eq!(
        tool_result,
        (
            OpId(1),
            "ask_user".to_string(),
            "call-ask".to_string(),
            json!({ "answer": "staging" }),
            1,
        )
    );

    // The resumed turn's projection feeds the answer back to the model as an
    // ordinary tool result, adjacent to the tool call that asked.
    let resumed_request = commands
        .iter()
        .filter_map(|cmd| match cmd {
            Command::StartModelCall {
                op: OpId(2),
                request,
                ..
            } => Some(request),
            _ => None,
        })
        .next()
        .expect("resumed model request");
    let roles: Vec<Role> = resumed_request.blocks.iter().map(|b| b.role).collect();
    assert_eq!(roles[..3], [Role::User, Role::Assistant, Role::Tool]);
    assert!(matches!(
        resumed_request.blocks[2].content.as_slice(),
        [ContentPart::ToolResult { id, .. }] if id == "call-ask"
    ));
}

/// Determinism: re-feeding the identical event stream (including the
/// `UserAnswer`) to a fresh brain yields identical commands and an identical
/// durable log — the AskUser path is replayable like every other control-flow
/// path (ARCHITECTURE §6).
#[test]
fn ask_user_round_trip_replays_identically() {
    let script = ask_user_script();

    let mut brain_a = Brain::with_default_policy();
    let commands_a = run_script(&mut brain_a, script.clone());

    let mut brain_b = Brain::with_default_policy();
    let commands_b = run_script(&mut brain_b, script);

    assert_eq!(
        commands_a, commands_b,
        "replaying the same AskUser event stream must yield identical commands"
    );
    assert_eq!(brain_a.state().log(), brain_b.state().log());
}
