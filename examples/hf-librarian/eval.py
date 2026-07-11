"""Evaluate the hugr-docs agent against the published docs-QA dataset.

Downloads the dataset from the Hub, asks the hugr-docs agent (in-process, through its typed Python wheel) every question, and grades each answer with a judge agent defined on the `hugr-agents` surface.
"""

import json
import sys
from pathlib import Path

import hugr_agents as hugr
import hugr_docs
from huggingface_hub import HfApi, hf_hub_download

DOCS = Path(__file__).resolve().parents[2] / "docs"

REPO_ID = f"{HfApi().whoami()['name']}/hugr-docs-qa"

judge = hugr.Agent(
    name="qa-judge",
    description="Grades a candidate answer against the expected one.",
    system=(
        "You grade documentation answers. Given a question, the expected answer, and a candidate answer, decide "
        "whether the candidate conveys the expected facts. Judge meaning, not wording; extra correct detail is fine. "
        "Respond only with the structured JSON response requested by the provider schema."
    ),
    models={
        "base_url": "https://router.huggingface.co/v1",
        "api_key_env": "HUGR_API_KEY",
        "default": "medium",
        "medium": {
            "model": "google/gemma-4-31B-it:cerebras",
            "temperature": 0.0,
            "input_usd_per_m_tokens": 1.0,
            "output_usd_per_m_tokens": 1.5,
        },
    },
    response_schema={
        "type": "object",
        "properties": {"correct": {"type": "boolean"}, "reasoning": {"type": "string"}},
        "required": ["correct", "reasoning"],
        "additionalProperties": False,
    },
)


def main() -> None:
    data = hf_hub_download(REPO_ID, "data/qa.jsonl", repo_type="dataset")
    items = [json.loads(line) for line in Path(data).read_text().splitlines()]
    print(f"evaluating hugr-docs on {len(items)} questions from {REPO_ID}\n")

    correct, cost = 0, 0
    for number, item in enumerate(items, 1):
        answered = hugr_docs.ask(str(DOCS), item["question"])
        if not answered.ok:
            sys.exit(f"hugr-docs failed (trace {answered.trace_id}): {answered.error}")
        candidate = answered.response.response

        graded = judge.ask(
            json.dumps(
                {"question": item["question"], "expected": item["expected_answer"], "candidate": candidate}
            )
        )
        if not graded.ok:
            sys.exit(f"judge failed (trace {graded.trace_id}): {graded.response.get('error')}")
        ok = graded.response["correct"]
        correct += ok
        cost += answered.metadata.cost_micro_usd + graded.metadata.cost_micro_usd

        print(f"{number:2}. {'PASS' if ok else 'FAIL'} [{item['difficulty']}] {item['question']}")
        if not ok:
            print(f"      expected: {item['expected_answer']}")
            print(f"      got:      {candidate}")
            print(f"      judge:    {graded.response['reasoning']}")

    print(f"\nscore: {correct}/{len(items)} — eval cost: {cost} µUSD")


if __name__ == "__main__":
    main()
