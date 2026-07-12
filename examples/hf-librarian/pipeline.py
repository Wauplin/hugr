"""Generate a docs-QA dataset with the hugr-datasmith agent, then publish it to the Hugging Face Hub.

The datasmith runs in-process through its typed Python wheel. The librarian is defined right here on the `hugr-agents` surface, and its entire tool surface is three functions bound to one dataset repo — even with full Hub credentials in the environment, the model cannot touch anything else.
"""

import json
import sys
from collections import Counter
from dataclasses import asdict
from pathlib import Path

import hugr_agents as hugr
import hugr_datasmith
from huggingface_hub import HfApi

COUNT = 10
DOCS = Path(__file__).resolve().parents[2] / "docs"
STAGED = Path(__file__).resolve().parent / "qa.jsonl"

api = HfApi()
REPO_ID = f"{api.whoami()['name']}/hugr-docs-qa"


@hugr.tool
def dataset_summary() -> dict:
    """Statistics and sample rows of the staged dataset. Call this first."""
    items = [json.loads(line) for line in STAGED.read_text().splitlines()]
    return {
        "repo_id": REPO_ID,
        "count": len(items),
        "difficulties": dict(Counter(item["difficulty"] for item in items)),
        "source_files": sorted({item["source_path"] for item in items}),
        "samples": items[:3],
    }


@hugr.tool
def upload_readme(content: str) -> str:
    """Upload the dataset card (markdown with YAML front matter) as the repo's README.md."""
    if not content.startswith("---"):
        return "rejected: the card must start with YAML front matter (---)"
    api.upload_file(
        path_or_fileobj=content.encode(), path_in_repo="README.md", repo_id=REPO_ID, repo_type="dataset"
    )
    return f"uploaded README.md to {REPO_ID}"


@hugr.tool
def publish_data() -> str:
    """Upload the staged JSONL data file as data/qa.jsonl of the repo."""
    api.upload_file(
        path_or_fileobj=STAGED, path_in_repo="data/qa.jsonl", repo_id=REPO_ID, repo_type="dataset"
    )
    return f"uploaded data/qa.jsonl to {REPO_ID}"


librarian = hugr.Agent(
    name="hf-librarian",
    description="Publishes one staged dataset to one Hugging Face dataset repo.",
    system=(
        "You are hf-librarian, a Hugging Face Hub publishing specialist. Call `dataset_summary` first and ground "
        "everything you write in it. Then write a proper dataset card and upload it with `upload_readme`: YAML front "
        "matter (license, task_categories, tags including `hugr` and `synthetic`), a short summary, a field-by-field "
        "schema description, how the data was generated (synthesized by a jailed docs-mining agent), the intended use "
        "(evaluating documentation assistants), and honest limitations (synthetic, unreviewed). Finally call "
        "`publish_data`. Respond only with the structured JSON response requested by the provider schema."
    ),
    models={
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "HUGR_API_KEY",
        "default": "medium",
        "medium": {
            "model": "google/gemma-4-31B-it:cerebras",
            "input_usd_per_m_tokens": 1.0,
            "output_usd_per_m_tokens": 1.5,
        },
    },
    tools=[dataset_summary, upload_readme, publish_data],
    response_schema={
        "type": "object",
        "properties": {"notes": {"type": "string"}},
        "required": ["notes"],
        "additionalProperties": False,
    },
)


def main() -> None:
    print(f"[1/2] datasmith: mining {DOCS} for {COUNT} Q&A pairs...")
    generated = hugr_datasmith.ask(
        str(DOCS), f"Generate {COUNT} question/answer pairs covering the whole documentation set."
    )
    if not generated.ok:
        sys.exit(f"datasmith failed (trace {generated.trace_id}): {generated.error}")
    dataset = generated.response
    print(f"      {len(dataset.items)} pairs, coverage: {dataset.coverage}")
    STAGED.write_text("".join(json.dumps(asdict(item)) + "\n" for item in dataset.items))

    print(f"[2/2] hf-librarian: publishing to {REPO_ID}...")
    api.create_repo(REPO_ID, repo_type="dataset", exist_ok=True)
    published = librarian.ask(f"Publish the staged docs-QA dataset to {REPO_ID}.")
    if not published.ok:
        sys.exit(f"librarian failed (trace {published.trace_id}): {published.response.get('error')}")
    print(f"      {published.response['notes']}")

    cost = generated.metadata.cost_micro_usd + published.metadata.cost_micro_usd
    print(f"\ncost: {cost} µUSD — traces: datasmith={generated.trace_id} librarian={published.trace_id}")
    print(f"dataset: https://huggingface.co/datasets/{REPO_ID}")


if __name__ == "__main__":
    main()
