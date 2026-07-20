"""LLM-backed generation of grounded HF documentation questions."""

from __future__ import annotations

import hashlib
import json
import re
from collections import defaultdict
from pathlib import Path
from typing import Any

from .dataset import DIFFICULTIES, fingerprint_docs, prepare_items

MAX_READ_BYTES = 250_000


def _jailed_path(root: Path, relative: str) -> Path:
    candidate = (root / relative.removeprefix("docs/").lstrip("/")).resolve()
    try:
        candidate.relative_to(root)
    except ValueError as error:
        raise ValueError(f"path escapes docs root: {relative}") from error
    return candidate


def _agent(root: Path, model: str, difficulty: str, item_count: int, allowed_sources: list[str]):
    import huggr_agents as huggr

    @huggr.tool
    def read_docs(paths: list[str]) -> dict:
        """Read up to eight authoritative Markdown pages from the host-selected source set."""
        if not 1 <= len(paths) <= 8:
            return {"error": "provide between one and eight paths"}
        documents = []
        for relative in paths:
            relative = relative.removeprefix("docs/").lstrip("/")
            if relative not in allowed_sources:
                return {"error": f"source is outside this batch's allowed set: {relative}"}
            target = _jailed_path(root, relative)
            if target.name == "AI_INDEX.md" or not target.is_file() or target.suffix != ".md":
                return {"error": f"not an authoritative Markdown page: {relative}"}
            documents.append({"path": target.relative_to(root).as_posix(), "content": target.read_text(errors="replace")[:MAX_READ_BYTES]})
        return {"documents": documents}

    schema = {
        "type": "object",
        "properties": {
            "items": {
                "type": "array",
                "minItems": item_count,
                "maxItems": item_count,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string"},
                        "expected_answer": {"type": "string"},
                        "source_paths": {
                            "type": "array",
                            "items": {"type": "string", "enum": allowed_sources},
                            "minItems": 1,
                        },
                        "difficulty": {"type": "string", "enum": [difficulty]},
                        "topic": {"type": "string"},
                        "rubric": {"type": "string"},
                    },
                    "required": ["question", "expected_answer", "source_paths", "difficulty", "topic", "rubric"],
                    "additionalProperties": False,
                },
            },
            "coverage": {"type": "string"},
        },
        "required": ["items", "coverage"],
        "additionalProperties": False,
    }
    return huggr.Agent(
        name="hf-docs-dataset-generator",
        description="Generates grounded HF documentation evaluation questions.",
        system=(
            "Generate evaluation questions from a host-selected set of Hugging Face documentation pages. Read every source "
            "named by an item. Questions must sound like real user requests and must "
            "not reveal filenames or quote a section heading as a clue. Expected answers must be self-contained and fully "
            "supported by source_paths. Basic items test one explicit fact or procedure. Intermediate items combine related "
            "details from one or two pages. Advanced items test edge cases, tradeoffs, interactions, or product distinctions. "
            "Do not use AI_INDEX.md as a source. Spread questions across products and avoid near duplicates. The rubric must "
            "state the facts a correct answer must contain. Respond only with the requested structured object."
        ),
        models={"default": model},
        tools=[read_docs],
        limits={"max_model_calls": 12, "timeout_s": 300},
        response_schema=schema,
    )


def _source_batch(root: Path, difficulty: str, seed: str, batch_number: int, limit: int = 14) -> list[str]:
    grouped: dict[str, list[str]] = defaultdict(list)
    for path in sorted(root.rglob("*.md")):
        relative = path.relative_to(root).as_posix()
        if path.name == "AI_INDEX.md" or not path.is_file():
            continue
        if difficulty == "basic" and "/package_reference/" in f"/{relative}":
            continue
        grouped[relative.split("/", 1)[0]].append(relative)
    products = sorted(grouped, key=lambda product: hashlib.sha256(f"{seed}:{difficulty}:{batch_number}:{product}".encode()).hexdigest())
    selected: list[str] = []
    round_number = 0
    while len(selected) < limit and products:
        added = False
        for product in products:
            paths = sorted(grouped[product], key=lambda path: hashlib.sha256(f"{seed}:{difficulty}:{path}".encode()).hexdigest())
            index = batch_number * 2 + round_number
            if index < len(paths):
                selected.append(paths[index])
                added = True
                if len(selected) == limit:
                    break
        if not added:
            break
        round_number += 1
    if len(selected) < 5:
        raise ValueError(f"not enough source pages to generate a {difficulty} batch")
    return selected


def _answer_error(answer: Any) -> str:
    direct = getattr(answer, "error", None)
    if direct:
        return str(direct)
    if isinstance(answer.response, dict):
        return str(answer.response.get("error", "unknown generator error"))
    return "unknown generator error"


def _write_checkpoint(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(payload, indent=2, ensure_ascii=False, sort_keys=True) + "\n")
    temporary.replace(path)


def generate_items(
    docs_root: Path,
    counts: dict[str, int],
    model: str,
    seed: str,
    checkpoint_path: Path | None = None,
) -> tuple[list[dict[str, Any]], dict[str, Any]]:
    root = docs_root.resolve()
    if not (root / "AI_INDEX.md").is_file():
        raise ValueError(f"HF documentation root is missing AI_INDEX.md: {root}")
    docs_sha256 = fingerprint_docs(root)["sha256"]
    if checkpoint_path and checkpoint_path.exists():
        checkpoint = json.loads(checkpoint_path.read_text())
        if (
            checkpoint["counts"] != counts
            or checkpoint["model"] != model
            or checkpoint["seed"] != seed
            or checkpoint["docs_sha256"] != docs_sha256
        ):
            raise ValueError(f"generation checkpoint configuration differs from this run: {checkpoint_path}")
        items = checkpoint["items"]
        traces = checkpoint["traces"]
        cost = checkpoint["cost_micro_usd"]
    else:
        items = []
        traces = []
        cost = 0
    prior_questions = [item["question"] for item in items]
    for difficulty in DIFFICULTIES:
        target_count = counts.get(difficulty, 0)
        completed_count = sum(item["difficulty"] == difficulty for item in items)
        if completed_count > target_count:
            raise ValueError(f"generation checkpoint has too many {difficulty} items")
        remaining = target_count - completed_count
        if remaining <= 0:
            continue
        prior = "\n".join(f"- {question}" for question in prior_questions[-100:]) or "(none)"
        batch_number = completed_count // 5
        while remaining:
            batch_number += 1
            batch_size = min(5, remaining)
            allowed_sources = _source_batch(root, difficulty, seed, batch_number)
            generated = None
            last_error = ""
            for attempt in range(1, 3):
                agent = _agent(root, model, difficulty, batch_size, allowed_sources)
                answer = agent.ask(
                    f"Generate exactly {batch_size} {difficulty} question/answer pairs for batch {batch_number}. You may use "
                    f"only these source paths and must read every page you cite:\n- "
                    + "\n- ".join(allowed_sources)
                    + f"\nDo not repeat or closely paraphrase these existing questions:\n{prior}"
                    + (f"\nThe prior attempt failed host validation: {last_error}" if last_error else "")
                )
                traces.append(
                    {
                        "difficulty": difficulty,
                        "batch": batch_number,
                        "attempt": attempt,
                        "trace_id": answer.trace_id,
                        "count": batch_size,
                        "ok": answer.ok,
                    }
                )
                cost += answer.metadata.cost_micro_usd
                if not answer.ok:
                    last_error = _answer_error(answer)
                    continue
                candidate = answer.response["items"]
                for item in candidate:
                    item["question"] = re.sub(r"\s+", " ", item["question"]).strip()
                try:
                    prepare_items(candidate, root)
                except ValueError as error:
                    last_error = str(error)
                    traces[-1]["validation_error"] = last_error
                    continue
                generated = candidate
                break
            if generated is None:
                raise RuntimeError(f"generator failed validation for {difficulty} batch {batch_number}: {last_error}")
            items.extend(generated)
            prior_questions.extend(item["question"] for item in generated)
            remaining -= batch_size
            prior = "\n".join(f"- {question}" for question in prior_questions[-100:])
            if checkpoint_path:
                _write_checkpoint(
                    checkpoint_path,
                    {
                        "schema_version": 1,
                        "counts": counts,
                        "model": model,
                        "seed": seed,
                        "docs_sha256": docs_sha256,
                        "items": items,
                        "traces": traces,
                        "cost_micro_usd": cost,
                    },
                )
    return items, {"model_tier": model, "traces": traces, "cost_micro_usd": cost}
