import json
from pathlib import Path

import pytest

from hf_docs_research.dataset import fingerprint_docs, load_split, prepare_items, split_items, write_dataset


def _docs(tmp_path: Path) -> Path:
    docs = tmp_path / "docs"
    (docs / "hub").mkdir(parents=True)
    (docs / "AI_INDEX.md").write_text("# Generated index")
    for number in range(6):
        (docs / "hub" / f"page-{number}.md").write_text(f"# Page {number}\nFact {number}.")
    return docs


def _items() -> list[dict]:
    items = []
    for difficulty, offset in (("basic", 0), ("intermediate", 2), ("advanced", 4)):
        for number in range(offset, offset + 2):
            items.append(
                {
                    "question": f"Question {number}?",
                    "expected_answer": f"Fact {number} is the documented behavior and this sentence makes the answer substantive.",
                    "source_paths": [f"hub/page-{number}.md"],
                    "difficulty": difficulty,
                    "topic": "hub",
                    "rubric": f"A correct answer must include documented fact {number} and explain its behavior.",
                }
            )
    return items


def test_fingerprint_changes_with_content(tmp_path: Path) -> None:
    docs = _docs(tmp_path)
    before = fingerprint_docs(docs)
    (docs / "hub" / "page-0.md").write_text("changed")
    after = fingerprint_docs(docs)
    assert before["sha256"] != after["sha256"]
    assert after["markdown_files"] == 7


def test_split_is_stratified_and_deterministic(tmp_path: Path) -> None:
    prepared = prepare_items(_items(), _docs(tmp_path))
    first = split_items(prepared, 0.5, "fixed")
    second = split_items(list(reversed(prepared)), 0.5, "fixed")
    assert first == second
    assert {item["difficulty"] for item in first[0]} == {"basic", "intermediate", "advanced"}
    assert {item["difficulty"] for item in first[1]} == {"basic", "intermediate", "advanced"}


def test_dataset_snapshot_is_immutable(tmp_path: Path) -> None:
    docs = _docs(tmp_path)
    target = write_dataset(_items(), docs, tmp_path / "datasets", "v1", 0.5, "fixed")
    manifest, test = load_split(target, "test")
    assert manifest["counts"]["total"] == 6
    assert len(test) == 3
    with pytest.raises(FileExistsError):
        write_dataset(_items(), docs, tmp_path / "datasets", "v1", 0.5, "fixed")


def test_navigation_indexes_are_rejected_as_sources(tmp_path: Path) -> None:
    docs = _docs(tmp_path)
    item = _items()[0]
    item["source_paths"] = ["AI_INDEX.md"]
    with pytest.raises(ValueError, match="navigation index"):
        prepare_items([item], docs)


def test_source_path_is_rejected_as_expected_answer(tmp_path: Path) -> None:
    docs = _docs(tmp_path)
    item = _items()[0]
    item["expected_answer"] = item["source_paths"][0]
    with pytest.raises(ValueError, match="not substantive"):
        prepare_items([item], docs)
