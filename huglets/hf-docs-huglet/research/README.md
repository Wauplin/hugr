# HF docs huglet research harness

This harness creates a fixed, difficulty-stratified benchmark and compares `hf-docs-huglet` variants on answer quality, source retrieval, latency, token use, tool use, and candidate cost. It stores detailed runs as JSON and generates a CSV plus PNG charts for comparisons.

It does not run an autonomous optimization loop. A later coding agent can use the commands here as its stable experiment boundary.

## What is fixed

`generate` calls a specialized Huggr dataset generator in bounded five-item batches. The host selects a deterministic, product-diverse set of existing source paths for each batch, embeds those paths as schema enums, and grants one read tool that accepts only the selected pages. This prevents the generator from citing invented or out-of-batch paths. Each pair contains a natural question, self-contained expected answer, source paths, difficulty, topic, and grading rubric.

The host validates every source, rejects `AI_INDEX.md`, hashes each source, assigns a content-derived pair id, and creates a deterministic stratified split. A snapshot folder contains:

```text
datasets/hf-docs-v1/
  manifest.json       # generation traces/cost, split seed, docs and dataset hashes
  train.jsonl         # visible during variant development
  test.jsonl          # held out until evaluation
```

Snapshot folders are immutable. The same test JSONL is used for every comparable variant. Daily dump changes do not rewrite a benchmark; evaluations record the current corpus fingerprint and changed or missing benchmark sources. Pass `--require-docs-match` when an exact corpus match is required.

## Environment

You need Rust, Python 3.10 or newer, `uv`, `maturin`, and `HF_TOKEN` for the configured Hugging Face model provider. From `huglets/hf-docs-huglet`:

```bash
uv venv research/.venv --python 3.12
uv pip install --python research/.venv/bin/python -e 'research[test]'

(cd ../../bindings/python && maturin build --release)
cargo run -p huggr-toolkit --bin huggr -- build . --surface python --release

uv pip install --python research/.venv/bin/python \
  ../../crates/huggr-python/target/wheels/huggr_agents-*.whl \
  dist/hf-docs-huglet-python/target/wheels/hf_docs_huglet-*.whl
```

Rebuild and reinstall the `hf_docs_huglet` wheel after changing `SYSTEM.md`, `huggr.toml`, or `src/lib.rs`. The candidate module and public API stay constant across variants.

## Generate the first snapshot

Generation incurs model cost and should happen only when creating a benchmark version. A useful first study has enough items in every difficulty for both splits:

```bash
export HF_TOKEN=hf_...
research/.venv/bin/hf-docs-research generate \
  --docs ../../hf-dump-light \
  --name hf-docs-v1 \
  --basic 30 \
  --intermediate 30 \
  --advanced 30 \
  --test-fraction 0.3 \
  --seed hf-docs-v1
```

Generation fails instead of replacing an existing snapshot. If validation finds duplicate questions, missing sources, generated-index citations, a wrong item count, or invalid difficulty labels, fix the generator and use a new snapshot name.

Create a separate full-dump benchmark when package-reference behavior is itself under study. Do not mix questions generated from different corpus fingerprints into one snapshot without recording that as a new benchmark design.

## Evaluate a variant

The evaluator imports the installed `hf_docs_huglet` wheel and calls `ask(docs_path, question)` for every item. A separate tool-free judge grades the candidate against the expected answer and rubric on a 0 to 4 scale. Exact expected source paths produce a deterministic citation-recall metric independent of the judge.

```bash
research/.venv/bin/hf-docs-research evaluate \
  --docs ../../hf-dump-light \
  --dataset research/datasets/hf-docs-v1 \
  --split test \
  --variant baseline
```

Use `--split train` for exploratory runs and `--limit N` for smoke checks. The full fixed test split is the comparable acceptance run. Use `--candidate-module` only when deliberately testing a differently named package; normal variants retain `hf_docs_huglet`.

Each result records per-case candidate and judge trace ids, models, answer text, cited paths, verdict, latency, tokens, tool calls, and cost. The summary separates candidate cost from judge cost so changing the judge provider does not make a candidate look cheaper or more expensive.

## Draw charts

```bash
research/.venv/bin/hf-docs-research report
```

The command reads every result JSON directly under `research/results/` and writes:

- `results/charts/summary.csv` for analysis in a spreadsheet or notebook;
- `results/charts/overview.png` for accuracy, source recall, cost, and p50 latency;
- `results/charts/quality-cost.png` for the quality/cost tradeoff across variants.

Keep the detailed JSON result and regenerated charts with the variant commit. Results produced with a different dataset hash, split, or benchmark method are not directly comparable even when the chart can display them.

The checked-in `baseline-v0.1.0` run is the first reference point: all nine held-out cases completed, answer accuracy was 66.7%, required-source recall was 88.9%, candidate cost was 236,759 µUSD, and candidate p50 latency was 3.3 seconds. The result also records 44 candidate model calls and 35 tool calls, which gives later research a concrete efficiency baseline.

## Validate the harness

```bash
research/.venv/bin/pytest research/tests
```

These tests cover corpus fingerprinting, source validation, immutable snapshots, deterministic stratified splitting, and report summaries. The benchmark evaluation remains the acceptance gate for the huglet itself.
