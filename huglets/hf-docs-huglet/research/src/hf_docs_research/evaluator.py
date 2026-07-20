"""Evaluate one installed hf-docs-huglet variant on a fixed dataset split."""

from __future__ import annotations

import importlib
import json
import math
import statistics
import subprocess
from collections import defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .dataset import fingerprint_docs, load_split, source_drift


def _judge(model: str):
    import huggr_agents as huggr

    return huggr.Agent(
        name="hf-docs-qa-judge",
        description="Grades answers from HF documentation huglet variants.",
        system=(
            "Grade a candidate answer to a Hugging Face documentation question against the reference answer and rubric. "
            "Judge meaning and required facts, not wording. Do not reward unsupported claims. A score of 4 is fully correct; "
            "3 is correct with a minor omission; 2 is partially correct but misses an important requirement; 1 has little "
            "correct content; 0 is incorrect or no answer. Set correct=true only for scores 3 or 4. Set grounded=false when "
            "the candidate materially contradicts the reference or makes claims that the supplied reference does not support. "
            "Respond only with the requested structured object."
        ),
        models={"default": model},
        response_schema={
            "type": "object",
            "properties": {
                "score": {"type": "integer", "minimum": 0, "maximum": 4},
                "correct": {"type": "boolean"},
                "grounded": {"type": "boolean"},
                "reasoning": {"type": "string"},
            },
            "required": ["score", "correct", "grounded", "reasoning"],
            "additionalProperties": False,
        },
    )


def _percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, max(0, math.ceil(fraction * len(ordered)) - 1))]


def _related_paths(response: Any) -> list[str]:
    related = response.related_documents if hasattr(response, "related_documents") else response["related_documents"]
    paths = []
    for document in related:
        paths.append(document.path if hasattr(document, "path") else document["path"])
    return paths


def _candidate_text(response: Any) -> str:
    return response.response if hasattr(response, "response") else response["response"]


def _answer_error(answer: Any) -> str:
    direct = getattr(answer, "error", None)
    if direct:
        return str(direct)
    if isinstance(answer.response, dict):
        return str(answer.response.get("error", "unknown agent error"))
    return "unknown agent error"


def _git_state(repo: Path) -> dict[str, Any]:
    try:
        commit = subprocess.run(["git", "rev-parse", "HEAD"], cwd=repo, check=True, capture_output=True, text=True).stdout.strip()
        dirty = bool(subprocess.run(["git", "status", "--porcelain"], cwd=repo, check=True, capture_output=True, text=True).stdout.strip())
        return {"commit": commit, "dirty": dirty}
    except (OSError, subprocess.CalledProcessError):
        return {"commit": None, "dirty": None}


def _aggregate(cases: list[dict[str, Any]]) -> dict[str, Any]:
    completed = [case for case in cases if case["status"] == "completed"]
    candidate_cost = sum(case["candidate"]["cost_micro_usd"] for case in cases if case.get("candidate"))
    judge_cost = sum(case.get("judge", {}).get("cost_micro_usd", 0) for case in cases)
    durations = [case["candidate"]["duration_ms"] for case in cases if case.get("candidate")]

    def metrics(rows: list[dict[str, Any]]) -> dict[str, Any]:
        if not rows:
            return {"count": 0, "accuracy": 0.0, "mean_score": 0.0, "grounded_rate": 0.0, "citation_recall": 0.0}
        return {
            "count": len(rows),
            "accuracy": sum(bool(row["judge"]["correct"]) for row in rows) / len(rows),
            "mean_score": statistics.fmean(row["judge"]["score"] for row in rows),
            "grounded_rate": sum(bool(row["judge"]["grounded"]) for row in rows) / len(rows),
            "citation_recall": statistics.fmean(row["citation_recall"] for row in rows),
        }

    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for case in completed:
        grouped[case["difficulty"]].append(case)
    return {
        **metrics(completed),
        "attempted": len(cases),
        "failed": len(cases) - len(completed),
        "candidate_cost_micro_usd": candidate_cost,
        "judge_cost_micro_usd": judge_cost,
        "candidate_tokens_in": sum(case["candidate"]["tokens_in"] for case in cases if case.get("candidate")),
        "candidate_tokens_out": sum(case["candidate"]["tokens_out"] for case in cases if case.get("candidate")),
        "candidate_tool_calls": sum(case["candidate"]["tool_calls"] for case in cases if case.get("candidate")),
        "candidate_model_calls": sum(case["candidate"]["model_calls"] for case in cases if case.get("candidate")),
        "latency_ms": {
            "mean": statistics.fmean(durations) if durations else 0.0,
            "p50": _percentile(durations, 0.50),
            "p95": _percentile(durations, 0.95),
        },
        "by_difficulty": {difficulty: metrics(grouped[difficulty]) for difficulty in sorted(grouped)},
    }


def evaluate(
    docs_root: Path,
    dataset_dir: Path,
    split: str,
    candidate_module: str,
    variant: str,
    judge_model: str,
    repo_root: Path,
    limit: int | None = None,
    require_docs_match: bool = False,
) -> dict[str, Any]:
    manifest, items = load_split(dataset_dir, split)
    if limit is not None:
        items = items[:limit]
    current_docs = fingerprint_docs(docs_root)
    changed_sources = source_drift(items, docs_root)
    docs_match = current_docs["sha256"] == manifest["docs"]["sha256"]
    if require_docs_match and not docs_match:
        raise ValueError("documentation fingerprint differs from the dataset snapshot")
    candidate = importlib.import_module(candidate_module)
    judge = _judge(judge_model)
    cases = []
    for number, item in enumerate(items, 1):
        print(f"[{number}/{len(items)}] {item['difficulty']}: {item['question']}", flush=True)
        try:
            answered = candidate.ask(str(docs_root.resolve()), item["question"])
        except Exception as error:
            cases.append({"id": item["id"], "difficulty": item["difficulty"], "status": "candidate_exception", "error": str(error)})
            continue
        candidate_meta = {
            "trace_id": answered.trace_id,
            "answer": _candidate_text(answered.response) if answered.ok else None,
            "related_documents": _related_paths(answered.response) if answered.ok else [],
            "error": _answer_error(answered) if not answered.ok else None,
            "duration_ms": answered.metadata.duration_ms,
            "cost_micro_usd": answered.metadata.cost_micro_usd,
            "tokens_in": answered.metadata.tokens_in,
            "tokens_out": answered.metadata.tokens_out,
            "model_calls": answered.metadata.model_calls,
            "tool_calls": answered.metadata.tool_calls,
            "models": list(answered.metadata.models),
        }
        if not answered.ok:
            cases.append({"id": item["id"], "difficulty": item["difficulty"], "status": "candidate_error", "candidate": candidate_meta})
            continue
        cited = set(candidate_meta["related_documents"])
        expected_sources = set(item["source_paths"])
        citation_recall = len(cited & expected_sources) / len(expected_sources)
        graded = judge.ask(
            json.dumps(
                {
                    "question": item["question"],
                    "expected_answer": item["expected_answer"],
                    "rubric": item["rubric"],
                    "candidate_answer": candidate_meta["answer"],
                },
                ensure_ascii=False,
            )
        )
        if not graded.ok:
            cases.append({"id": item["id"], "difficulty": item["difficulty"], "status": "judge_error", "candidate": candidate_meta, "error": _answer_error(graded)})
            continue
        verdict = dict(graded.response)
        verdict.update({"trace_id": graded.trace_id, "cost_micro_usd": graded.metadata.cost_micro_usd, "models": list(graded.metadata.models)})
        cases.append(
            {
                "id": item["id"],
                "question": item["question"],
                "difficulty": item["difficulty"],
                "topic": item["topic"],
                "status": "completed",
                "citation_recall": citation_recall,
                "candidate": candidate_meta,
                "judge": verdict,
            }
        )
    return {
        "schema_version": 1,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "variant": variant,
        "candidate_module": candidate_module,
        "judge_model_tier": judge_model,
        "dataset": {"name": manifest["name"], "sha256": manifest["dataset_sha256"], "split": split, "items": len(items)},
        "docs": {
            "expected_sha256": manifest["docs"]["sha256"],
            "actual_sha256": current_docs["sha256"],
            "match": docs_match,
            "changed_sources": changed_sources,
        },
        "git": _git_state(repo_root),
        "summary": _aggregate(cases),
        "cases": cases,
    }


def store_result(result: dict[str, Any], output_root: Path) -> Path:
    output_root.mkdir(parents=True, exist_ok=True)
    timestamp = result["created_at"].replace(":", "").replace("-", "").split(".")[0].replace("+0000", "Z")
    variant = "".join(character if character.isalnum() or character in "-_" else "-" for character in result["variant"])
    target = output_root / f"{timestamp}-{variant}.json"
    suffix = 1
    while target.exists():
        target = output_root / f"{timestamp}-{variant}-{suffix}.json"
        suffix += 1
    target.write_text(json.dumps(result, indent=2, ensure_ascii=False, sort_keys=True) + "\n")
    return target
