//! Trace tooling over an agent's trace store: list the lineage forest, replay a trace step-by-step, and verify one bit-for-bit.
//!
//! The lineage renderer is pure over a `Vec<TraceHead>`, so it is unit-testable without touching disk.

use hugr_agent::{TraceHead, TraceListing};

/// Render the lineage forest of a set of trace heads as an indented tree.
/// Roots are traces whose `depends_on` is absent (or points outside this set);
/// children nest under their parent. Order is deterministic (by `trace_id`).
pub fn render_lineage(heads: &[TraceHead]) -> String {
    if heads.is_empty() {
        return "(no traces)".to_string();
    }
    let ids: std::collections::BTreeSet<&str> = heads.iter().map(|h| h.trace_id.as_str()).collect();

    // parent id → child heads, and the roots.
    let mut children: std::collections::BTreeMap<&str, Vec<&TraceHead>> =
        std::collections::BTreeMap::new();
    let mut roots: Vec<&TraceHead> = Vec::new();
    for head in heads {
        match &head.depends_on {
            Some(parent) if ids.contains(parent.as_str()) => {
                children.entry(parent.as_str()).or_default().push(head);
            }
            // No parent, or a parent that isn't in this store → a root.
            _ => roots.push(head),
        }
    }
    roots.sort_by(|a, b| a.trace_id.as_str().cmp(b.trace_id.as_str()));

    let mut out = String::new();
    for root in roots {
        render_node(root, &children, 0, &mut out);
    }
    out.trim_end().to_string()
}

pub fn render_lineage_with_feedback(heads: &[TraceListing]) -> String {
    if heads.is_empty() {
        return "(no traces)".to_string();
    }
    let ids: std::collections::BTreeSet<&str> = heads.iter().map(|h| h.trace_id.as_str()).collect();

    let mut children: std::collections::BTreeMap<&str, Vec<&TraceListing>> =
        std::collections::BTreeMap::new();
    let mut roots: Vec<&TraceListing> = Vec::new();
    for head in heads {
        match &head.depends_on {
            Some(parent) if ids.contains(parent.as_str()) => {
                children.entry(parent.as_str()).or_default().push(head);
            }
            _ => roots.push(head),
        }
    }
    roots.sort_by(|a, b| a.trace_id.as_str().cmp(b.trace_id.as_str()));

    let mut out = String::new();
    for root in roots {
        render_node_with_feedback(root, &children, 0, &mut out);
    }
    out.trim_end().to_string()
}

fn render_node(
    head: &TraceHead,
    children: &std::collections::BTreeMap<&str, Vec<&TraceHead>>,
    depth: usize,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let bullet = if depth == 0 { "•" } else { "└─" };
    out.push_str(&format!(
        "{indent}{bullet} {id} [{status}] {question}\n",
        id = head.trace_id.as_str(),
        status = head.status,
        question = truncate(&head.question, 60),
    ));
    if let Some(kids) = children.get(head.trace_id.as_str()) {
        let mut kids = kids.clone();
        kids.sort_by(|a, b| a.trace_id.as_str().cmp(b.trace_id.as_str()));
        for kid in kids {
            render_node(kid, children, depth + 1, out);
        }
    }
}

fn render_node_with_feedback(
    head: &TraceListing,
    children: &std::collections::BTreeMap<&str, Vec<&TraceListing>>,
    depth: usize,
    out: &mut String,
) {
    let indent = "  ".repeat(depth);
    let bullet = if depth == 0 { "•" } else { "└─" };
    out.push_str(&format!(
        "{indent}{bullet} {id} [{status}] feedback={feedback} {question}\n",
        id = head.trace_id.as_str(),
        status = head.status,
        feedback = head.feedback_count,
        question = truncate(&head.question, 60),
    ));
    if let Some(kids) = children.get(head.trace_id.as_str()) {
        let mut kids = kids.clone();
        kids.sort_by(|a, b| a.trace_id.as_str().cmp(b.trace_id.as_str()));
        for kid in kids {
            render_node_with_feedback(kid, children, depth + 1, out);
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim().replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hugr_agent::TraceId;

    fn head(id: &str, parent: Option<&str>, question: &str) -> TraceHead {
        TraceHead::new(
            TraceId::new(id.to_string()),
            parent.map(|p| TraceId::new(p.to_string())),
            "a",
            "0",
            None,
            question,
            "success",
        )
    }

    fn listing(
        id: &str,
        parent: Option<&str>,
        question: &str,
        feedback_count: u64,
    ) -> TraceListing {
        TraceListing::new(head(id, parent, question), feedback_count)
    }

    #[test]
    fn empty_store_renders_placeholder() {
        assert_eq!(render_lineage(&[]), "(no traces)");
    }

    #[test]
    fn fork_tree_renders_as_a_tree() {
        // root → t1 → { t2a, t2b }
        let heads = vec![
            head("root", None, "start"),
            head("t1", Some("root"), "follow up"),
            head("t2a", Some("t1"), "what-if A"),
            head("t2b", Some("t1"), "what-if B"),
        ];
        let tree = render_lineage(&heads);
        let lines: Vec<&str> = tree.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("• root"), "{tree}");
        assert!(
            lines[1].contains("t1") && lines[1].starts_with("  "),
            "{tree}"
        );
        // t2a and t2b nest one level deeper than t1, sorted.
        assert!(
            lines[2].contains("t2a") && lines[2].starts_with("    "),
            "{tree}"
        );
        assert!(
            lines[3].contains("t2b") && lines[3].starts_with("    "),
            "{tree}"
        );
    }

    #[test]
    fn orphan_child_becomes_a_root() {
        // A child whose parent isn't in this store still lists (as a root).
        let heads = vec![head("child", Some("missing-parent"), "q")];
        let tree = render_lineage(&heads);
        assert!(tree.starts_with("• child"), "{tree}");
    }

    #[test]
    fn feedback_counts_render_in_lineage() {
        let tree = render_lineage_with_feedback(&[listing("root", None, "start", 2)]);
        assert!(tree.contains("feedback=2"), "{tree}");
    }
}
