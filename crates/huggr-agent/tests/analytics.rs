use std::sync::Arc;

use huggr_agent::{
    Answer, AnswerMeta, Feedback, FeedbackBackend, MemFeedbackStore, MemTraceStore, Pricing,
    STATUS_SUCCESS, StatsOptions, TraceBackend, TraceHeader, TraceId, collect_stats,
};
use huggr_core::{LogEntry, OpId, OpMeta, OpOutcome, Record, Seq, Timestamp};
use huggr_replay::Trace;
use serde_json::{Value, json};

#[tokio::test]
async fn stats_fold_models_tools_child_cost_and_feedback() {
    let traces = Arc::new(MemTraceStore::new());
    let feedback = Arc::new(MemFeedbackStore::new());
    let pricing = Pricing::new().with_tier("medium", 2.0, 5.0);

    let trace_id = traces
        .put(
            Trace::new(
                Vec::new(),
                vec![
                    model_end(0, 1, "medium", 10, 4),
                    tool_result(1, 2, "fs_read", json!({ "error": "denied" })),
                    tool_end(2, 2, 4, 9, OpOutcome::Error(json!({ "error": "denied" }))),
                    tool_result(
                        3,
                        3,
                        "agent_child",
                        serde_json::to_value(child_answer("child-trace", 17)).unwrap(),
                    ),
                    tool_end(4, 3, 10, 12, OpOutcome::Ok),
                ],
                Some(1),
            ),
            TraceHeader::new("stats", "0.1.0", "q", STATUS_SUCCESS),
        )
        .await
        .unwrap();
    feedback
        .append(Feedback::new(trace_id.clone(), json!({ "score": 1 })))
        .await
        .unwrap();

    let stats = collect_stats(traces, feedback, &pricing, StatsOptions::new())
        .await
        .unwrap();

    assert_eq!(stats.ask_count, 1);
    assert_eq!(stats.feedback_count, 1);
    assert_eq!(stats.totals.model_calls, 1);
    assert_eq!(stats.totals.tool_calls, 2);
    assert_eq!(stats.totals.tokens_in, 10);
    assert_eq!(stats.totals.tokens_out, 4);
    assert_eq!(stats.totals.cost_own_micro_usd, 40);
    assert_eq!(stats.totals.cost_delegated_micro_usd, 17);
    assert_eq!(stats.totals.cost_micro_usd, 57);
    assert_eq!(stats.models[0].selector, "medium");
    assert_eq!(
        stats
            .tools
            .iter()
            .find(|tool| tool.name == "fs_read")
            .unwrap()
            .error_count,
        1
    );
    assert_eq!(stats.children[0].name, "child");
    assert_eq!(stats.children[0].cost_delegated_micro_usd, 17);
}

#[tokio::test]
async fn stats_for_resumed_trace_only_count_new_suffix() {
    let traces = Arc::new(MemTraceStore::new());
    let feedback = Arc::new(MemFeedbackStore::new());
    let pricing = Pricing::new().with_tier("medium", 1.0, 1.0);

    let parent_log = vec![model_end(0, 1, "medium", 100, 100)];
    let parent_id = traces
        .put(
            Trace::new(Vec::new(), parent_log.clone(), Some(1)),
            TraceHeader::new("stats", "0.1.0", "parent", STATUS_SUCCESS),
        )
        .await
        .unwrap();
    let mut child_log = parent_log;
    child_log.push(model_end(1, 2, "medium", 3, 4));
    let child_id = traces
        .put(
            Trace::new(Vec::new(), child_log, Some(2)),
            TraceHeader::new("stats", "0.1.0", "child", STATUS_SUCCESS).with_depends_on(parent_id),
        )
        .await
        .unwrap();

    let stats = collect_stats(
        traces,
        feedback,
        &pricing,
        StatsOptions::new().trace(child_id),
    )
    .await
    .unwrap();

    assert_eq!(stats.ask_count, 1);
    assert_eq!(stats.totals.model_calls, 1);
    assert_eq!(stats.totals.tokens_in, 3);
    assert_eq!(stats.totals.tokens_out, 4);
    assert_eq!(stats.totals.cost_micro_usd, 7);
}

fn model_end(seq: u64, op: u64, selector: &str, input: u64, output: u64) -> LogEntry {
    LogEntry::new(
        Seq(seq),
        Timestamp(seq),
        Record::OpEnded {
            op: OpId(op),
            outcome: OpOutcome::Ok,
            meta: op_meta(
                seq * 10,
                seq * 10 + 3,
                Some(selector),
                Some((input, output)),
            ),
        },
    )
}

fn tool_result(seq: u64, op: u64, name: &str, result: Value) -> LogEntry {
    LogEntry::new(
        Seq(seq),
        Timestamp(seq),
        Record::ToolResult {
            op: OpId(op),
            name: name.to_string(),
            call_id: format!("call-{op}"),
            result,
            est_tokens: 0,
        },
    )
}

fn tool_end(seq: u64, op: u64, started: u64, ended: u64, outcome: OpOutcome) -> LogEntry {
    LogEntry::new(
        Seq(seq),
        Timestamp(seq),
        Record::OpEnded {
            op: OpId(op),
            outcome,
            meta: op_meta(started, ended, None, None),
        },
    )
}

fn op_meta(
    started_at: u64,
    ended_at: u64,
    model: Option<&str>,
    usage: Option<(u64, u64)>,
) -> OpMeta {
    serde_json::from_value(json!({
        "started_at": started_at,
        "ended_at": ended_at,
        "model": model,
        "usage": usage.map(|(input_tokens, output_tokens)| json!({
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "extra": null
        })),
        "extra": null
    }))
    .unwrap()
}

fn child_answer(id: &str, cost_micro_usd: u64) -> Answer {
    Answer {
        status: STATUS_SUCCESS.to_string(),
        response: json!({ "text": "child" }),
        trace_id: TraceId::new(id),
        blobs: Vec::new(),
        metadata: AnswerMeta {
            cost_micro_usd,
            model_calls: 1,
            ..AnswerMeta::default()
        },
        extra: Value::Null,
    }
}
