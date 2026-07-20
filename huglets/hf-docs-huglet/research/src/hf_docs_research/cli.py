"""Command-line entry point for the HF docs huglet research workflow."""

from __future__ import annotations

import argparse
from pathlib import Path

from .dataset import DIFFICULTIES, write_dataset
from .evaluator import evaluate, store_result
from .generator import generate_items
from .report import write_report

RESEARCH_ROOT = Path(__file__).resolve().parents[2]
PROJECT_ROOT = RESEARCH_ROOT.parent
REPO_ROOT = PROJECT_ROOT.parents[1]


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="hf-docs-research", description="Generate fixed datasets and evaluate hf-docs-huglet variants.")
    subcommands = parser.add_subparsers(dest="command", required=True)

    generate = subcommands.add_parser("generate", help="Generate one immutable, stratified dataset snapshot.")
    generate.add_argument("--docs", type=Path, required=True)
    generate.add_argument("--name", required=True, help="New snapshot folder name, for example hf-docs-v1.")
    generate.add_argument("--basic", type=int, default=30)
    generate.add_argument("--intermediate", type=int, default=30)
    generate.add_argument("--advanced", type=int, default=30)
    generate.add_argument("--test-fraction", type=float, default=0.3)
    generate.add_argument("--seed", default="hf-docs-v1")
    generate.add_argument("--generator-model", choices=("fast", "balanced", "powerful", "max"), default="powerful")
    generate.add_argument("--output", type=Path, default=RESEARCH_ROOT / "datasets")

    evaluate_parser = subcommands.add_parser("evaluate", help="Evaluate an installed candidate wheel on a fixed split.")
    evaluate_parser.add_argument("--docs", type=Path, required=True)
    evaluate_parser.add_argument("--dataset", type=Path, required=True)
    evaluate_parser.add_argument("--split", choices=("train", "test"), default="test")
    evaluate_parser.add_argument("--candidate-module", default="hf_docs_huglet")
    evaluate_parser.add_argument("--variant", required=True)
    evaluate_parser.add_argument("--judge-model", choices=("fast", "balanced", "powerful", "max"), default="powerful")
    evaluate_parser.add_argument("--limit", type=int)
    evaluate_parser.add_argument("--require-docs-match", action="store_true")
    evaluate_parser.add_argument("--output", type=Path, default=RESEARCH_ROOT / "results")

    report = subcommands.add_parser("report", help="Regenerate CSV and PNG comparisons from stored result JSON.")
    report.add_argument("--results", type=Path, default=RESEARCH_ROOT / "results")
    report.add_argument("--output", type=Path, default=RESEARCH_ROOT / "results" / "charts")
    return parser


def main() -> None:
    args = _parser().parse_args()
    if args.command == "generate":
        counts = {difficulty: getattr(args, difficulty) for difficulty in DIFFICULTIES}
        checkpoint = args.output / f".{args.name}.generation.json"
        items, generation = generate_items(args.docs, counts, args.generator_model, args.seed, checkpoint)
        target = write_dataset(items, args.docs, args.output, args.name, args.test_fraction, args.seed, generation)
        checkpoint.unlink(missing_ok=True)
        print(f"wrote immutable dataset snapshot: {target}")
        return
    if args.command == "evaluate":
        result = evaluate(
            docs_root=args.docs,
            dataset_dir=args.dataset,
            split=args.split,
            candidate_module=args.candidate_module,
            variant=args.variant,
            judge_model=args.judge_model,
            repo_root=REPO_ROOT,
            limit=args.limit,
            require_docs_match=args.require_docs_match,
        )
        target = store_result(result, args.output)
        summary = result["summary"]
        print(f"stored result: {target}")
        print(
            f"accuracy={summary['accuracy']:.1%} score={summary['mean_score']:.2f}/4 "
            f"citation_recall={summary['citation_recall']:.1%} candidate_cost={summary['candidate_cost_micro_usd']} µUSD"
        )
        return
    outputs = write_report(args.results, args.output)
    for output in outputs:
        print(output)


if __name__ == "__main__":
    main()
