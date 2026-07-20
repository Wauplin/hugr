"""Generate comparison tables and charts from stored evaluation runs."""

from __future__ import annotations

import csv
import json
from pathlib import Path
from typing import Any


def load_results(results_root: Path) -> list[dict[str, Any]]:
    return [json.loads(path.read_text()) for path in sorted(results_root.glob("*.json"))]


def summary_rows(results: list[dict[str, Any]]) -> list[dict[str, Any]]:
    rows = []
    for result in results:
        summary = result["summary"]
        count = max(1, summary["attempted"])
        rows.append(
            {
                "variant": result["variant"],
                "created_at": result["created_at"],
                "dataset": result["dataset"]["name"],
                "split": result["dataset"]["split"],
                "accuracy": summary["accuracy"],
                "mean_score": summary["mean_score"],
                "grounded_rate": summary["grounded_rate"],
                "citation_recall": summary["citation_recall"],
                "cost_usd_per_question": summary["candidate_cost_micro_usd"] / count / 1_000_000,
                "latency_p50_ms": summary["latency_ms"]["p50"],
                "latency_p95_ms": summary["latency_ms"]["p95"],
                "failed": summary["failed"],
            }
        )
    return rows


def write_report(results_root: Path, output_dir: Path) -> list[Path]:
    results = load_results(results_root)
    if not results:
        raise ValueError(f"no result JSON files found in {results_root}")
    rows = summary_rows(results)
    output_dir.mkdir(parents=True, exist_ok=True)
    table = output_dir / "summary.csv"
    with table.open("w", newline="") as handle:
        writer = csv.DictWriter(handle, fieldnames=list(rows[0]), lineterminator="\n")
        writer.writeheader()
        writer.writerows(rows)

    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    labels = [row["variant"] for row in rows]
    figure, axes = plt.subplots(2, 2, figsize=(12, 8), constrained_layout=True)
    panels = [
        ("accuracy", "Answer accuracy", (0, 1)),
        ("citation_recall", "Required-source recall", (0, 1)),
        ("cost_usd_per_question", "Candidate cost per question (USD)", None),
        ("latency_p50_ms", "Candidate p50 latency (ms)", None),
    ]
    for axis, (field, title, limits) in zip(axes.flat, panels):
        axis.bar(labels, [row[field] for row in rows], color="#ff9d00")
        axis.set_title(title)
        if limits:
            axis.set_ylim(*limits)
        axis.tick_params(axis="x", labelrotation=25)
        axis.grid(axis="y", alpha=0.25)
    overview = output_dir / "overview.png"
    figure.savefig(overview, dpi=160)
    plt.close(figure)

    figure, axis = plt.subplots(figsize=(8, 6), constrained_layout=True)
    for row in rows:
        axis.scatter(row["cost_usd_per_question"], row["accuracy"], s=80)
        axis.annotate(row["variant"], (row["cost_usd_per_question"], row["accuracy"]), xytext=(5, 5), textcoords="offset points")
    axis.set_xlabel("Candidate cost per question (USD)")
    axis.set_ylabel("Answer accuracy")
    axis.set_ylim(0, 1.02)
    axis.set_title("Quality and cost tradeoff")
    axis.grid(alpha=0.25)
    tradeoff = output_dir / "quality-cost.png"
    figure.savefig(tradeoff, dpi=160)
    plt.close(figure)
    return [table, overview, tradeoff]
