use huggr_agent::AgentStats;

/// Format an accounted micro-USD amount for display. Accounting stays in
/// micro-USD for precision; anything user-facing shows USD. A nonzero amount
/// under a penny renders as `<$0.01` so small spends never read as free.
pub fn format_usd(micro_usd: u64) -> String {
    if micro_usd == 0 {
        return "$0.00".to_string();
    }
    if micro_usd < 10_000 {
        return "<$0.01".to_string();
    }
    format!("${:.2}", micro_usd as f64 / 1_000_000.0)
}

pub fn render_stats(stats: &AgentStats) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "asks: {}  feedback: {}\n",
        stats.ask_count, stats.feedback_count
    ));
    out.push_str(&format!(
        "cost: total={} own={} delegated={}\n",
        format_usd(stats.totals.cost_micro_usd),
        format_usd(stats.totals.cost_own_micro_usd),
        format_usd(stats.totals.cost_delegated_micro_usd)
    ));
    out.push_str(&format!(
        "tokens: in={} out={}  calls: models={} tools={}\n",
        stats.totals.tokens_in,
        stats.totals.tokens_out,
        stats.totals.model_calls,
        stats.totals.tool_calls
    ));
    out.push_str(&format!(
        "duration_ms: mean={} median={} p95={}\n",
        stats.duration.mean_ms, stats.duration.median_ms, stats.duration.p95_ms
    ));
    if !stats.models.is_empty() {
        out.push_str("\nmodels:\n");
        for model in &stats.models {
            out.push_str(&format!(
                "  {} calls={} tokens_in={} tokens_out={} cost={}\n",
                model.selector,
                model.calls,
                model.tokens_in,
                model.tokens_out,
                format_usd(model.cost_micro_usd)
            ));
        }
    }
    if !stats.tools.is_empty() {
        out.push_str("\ntools:\n");
        for tool in &stats.tools {
            out.push_str(&format!(
                "  {} calls={} errors={} total_latency_ms={} mean_latency_ms={}\n",
                tool.name,
                tool.calls,
                tool.error_count,
                tool.total_latency_ms,
                tool.mean_latency_ms
            ));
        }
    }
    if !stats.children.is_empty() {
        out.push_str("\nchild agents:\n");
        for child in &stats.children {
            out.push_str(&format!(
                "  {} calls={} delegated_cost={}\n",
                child.name,
                child.calls,
                format_usd(child.cost_delegated_micro_usd)
            ));
        }
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use huggr_agent::{AgentStats, ModelStats, StatsTotals};

    #[test]
    fn formats_micro_usd_as_usd() {
        assert_eq!(format_usd(0), "$0.00");
        assert_eq!(format_usd(1), "<$0.01");
        assert_eq!(format_usd(9_999), "<$0.01");
        assert_eq!(format_usd(10_000), "$0.01");
        assert_eq!(format_usd(85_855), "$0.09");
        assert_eq!(format_usd(1_300_000), "$1.30");
    }

    #[test]
    fn renders_summary_and_sections() {
        let stats = AgentStats {
            ask_count: 2,
            feedback_count: 1,
            totals: StatsTotals {
                cost_micro_usd: 1_300_000,
                cost_own_micro_usd: 1_290_000,
                cost_delegated_micro_usd: 10_000,
                model_calls: 3,
                tool_calls: 4,
                ..StatsTotals::default()
            },
            models: vec![ModelStats {
                selector: "medium".to_string(),
                calls: 3,
                cost_micro_usd: 5,
                ..ModelStats::default()
            }],
            ..AgentStats::default()
        };
        let rendered = render_stats(&stats);
        assert!(rendered.contains("asks: 2  feedback: 1"));
        assert!(rendered.contains("cost: total=$1.30 own=$1.29 delegated=$0.01"));
        assert!(rendered.contains("medium calls=3"));
        assert!(rendered.contains("cost=<$0.01"));
    }
}
