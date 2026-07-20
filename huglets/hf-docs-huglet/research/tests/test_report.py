from hf_docs_research.report import summary_rows


def test_summary_rows_separate_candidate_cost() -> None:
    rows = summary_rows(
        [
            {
                "variant": "baseline",
                "created_at": "2026-01-01T00:00:00+00:00",
                "dataset": {"name": "v1", "split": "test"},
                "summary": {
                    "attempted": 2,
                    "accuracy": 0.5,
                    "mean_score": 2.5,
                    "grounded_rate": 1.0,
                    "citation_recall": 0.75,
                    "candidate_cost_micro_usd": 100_000,
                    "latency_ms": {"p50": 10, "p95": 20},
                    "failed": 0,
                },
            }
        ]
    )
    assert rows[0]["cost_usd_per_question"] == 0.05
    assert rows[0]["accuracy"] == 0.5
