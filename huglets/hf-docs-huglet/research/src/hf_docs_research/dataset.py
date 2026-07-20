"""Immutable dataset snapshots and deterministic train/test splitting."""

from __future__ import annotations

import hashlib
import json
from collections import Counter, defaultdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

DIFFICULTIES = ("basic", "intermediate", "advanced")


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()


def _sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def _markdown_files(root: Path) -> list[Path]:
    return sorted(path for path in root.rglob("*.md") if path.is_file())


def fingerprint_docs(root: Path) -> dict[str, Any]:
    root = root.resolve()
    files = []
    aggregate = hashlib.sha256()
    for path in _markdown_files(root):
        relative = path.relative_to(root).as_posix()
        digest = _sha256(path.read_bytes())
        files.append({"path": relative, "sha256": digest})
        aggregate.update(relative.encode())
        aggregate.update(b"\0")
        aggregate.update(digest.encode())
        aggregate.update(b"\0")
    if not files:
        raise ValueError(f"no Markdown files found under {root}")
    return {"sha256": aggregate.hexdigest(), "markdown_files": len(files), "files": files}


def _source_digest(docs_root: Path, source_path: str) -> str:
    if source_path.endswith("AI_INDEX.md"):
        raise ValueError(f"navigation index cannot be a source: {source_path}")
    candidate = (docs_root / source_path).resolve()
    try:
        candidate.relative_to(docs_root.resolve())
    except ValueError as error:
        raise ValueError(f"source escapes documentation root: {source_path}") from error
    if not candidate.is_file():
        raise ValueError(f"source does not exist: {source_path}")
    return _sha256(candidate.read_bytes())


def prepare_items(items: Iterable[dict[str, Any]], docs_root: Path) -> list[dict[str, Any]]:
    prepared = []
    seen_questions: set[str] = set()
    for raw in items:
        difficulty = str(raw["difficulty"]).strip().lower()
        if difficulty not in DIFFICULTIES:
            raise ValueError(f"unsupported difficulty: {difficulty}")
        question = " ".join(str(raw["question"]).split())
        expected = " ".join(str(raw["expected_answer"]).split())
        topic = " ".join(str(raw.get("topic", "")).split())
        rubric = " ".join(str(raw.get("rubric", "")).split())
        sources = sorted({str(path).removeprefix("docs/").lstrip("/") for path in raw["source_paths"]})
        if not question or not expected or not sources:
            raise ValueError("question, expected_answer, and source_paths must be non-empty")
        if len(expected) < 60 or expected in sources:
            raise ValueError(f"expected answer is not substantive: {question}")
        if len(rubric) < 40:
            raise ValueError(f"rubric is not substantive: {question}")
        normalized_question = question.casefold()
        if normalized_question in seen_questions:
            raise ValueError(f"duplicate question: {question}")
        seen_questions.add(normalized_question)
        source_hashes = {path: _source_digest(docs_root, path) for path in sources}
        identity = {
            "question": question,
            "expected_answer": expected,
            "source_paths": sources,
            "difficulty": difficulty,
            "topic": topic,
            "rubric": rubric,
        }
        prepared.append({"id": _sha256(_json_bytes(identity))[:16], **identity, "source_hashes": source_hashes})
    return prepared


def split_items(items: list[dict[str, Any]], test_fraction: float, seed: str) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    if not 0 < test_fraction < 1:
        raise ValueError("test_fraction must be between 0 and 1")
    grouped: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for item in items:
        grouped[item["difficulty"]].append(item)
    train: list[dict[str, Any]] = []
    test: list[dict[str, Any]] = []
    for difficulty in DIFFICULTIES:
        group = sorted(grouped[difficulty], key=lambda item: _sha256(f"{seed}:{item['id']}".encode()))
        if len(group) < 2:
            train.extend(group)
            continue
        test_count = min(len(group) - 1, max(1, round(len(group) * test_fraction)))
        test.extend(group[:test_count])
        train.extend(group[test_count:])
    return sorted(train, key=lambda item: item["id"]), sorted(test, key=lambda item: item["id"])


def _write_jsonl(path: Path, items: list[dict[str, Any]]) -> None:
    path.write_text("".join(json.dumps(item, ensure_ascii=False, sort_keys=True) + "\n" for item in items))


def write_dataset(
    items: Iterable[dict[str, Any]],
    docs_root: Path,
    output_root: Path,
    name: str,
    test_fraction: float,
    seed: str,
    generation: dict[str, Any] | None = None,
) -> Path:
    target = output_root / name
    if target.exists():
        raise FileExistsError(f"dataset snapshot already exists: {target}")
    prepared = prepare_items(items, docs_root.resolve())
    train, test = split_items(prepared, test_fraction, seed)
    if not test:
        raise ValueError("at least two items per represented difficulty are required to create a test split")
    target.mkdir(parents=True)
    _write_jsonl(target / "train.jsonl", train)
    _write_jsonl(target / "test.jsonl", test)
    docs = fingerprint_docs(docs_root)
    dataset_hash = _sha256((target / "train.jsonl").read_bytes() + (target / "test.jsonl").read_bytes())
    manifest = {
        "schema_version": 1,
        "name": name,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "seed": seed,
        "test_fraction": test_fraction,
        "dataset_sha256": dataset_hash,
        "docs": {"sha256": docs["sha256"], "markdown_files": docs["markdown_files"]},
        "counts": {
            "total": len(prepared),
            "train": len(train),
            "test": len(test),
            "by_difficulty": dict(sorted(Counter(item["difficulty"] for item in prepared).items())),
        },
        "generation": generation or {},
    }
    (target / "manifest.json").write_text(json.dumps(manifest, indent=2, ensure_ascii=False, sort_keys=True) + "\n")
    return target


def load_split(dataset_dir: Path, split: str) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    if split not in {"train", "test"}:
        raise ValueError("split must be train or test")
    manifest = json.loads((dataset_dir / "manifest.json").read_text())
    items = [json.loads(line) for line in (dataset_dir / f"{split}.jsonl").read_text().splitlines() if line]
    return manifest, items


def source_drift(items: list[dict[str, Any]], docs_root: Path) -> list[str]:
    changed = []
    for item in items:
        for source, expected_hash in item["source_hashes"].items():
            try:
                actual_hash = _source_digest(docs_root.resolve(), source)
            except ValueError:
                actual_hash = "missing"
            if actual_hash != expected_hash:
                changed.append(source)
    return sorted(set(changed))
