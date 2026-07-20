# AGENTS.md

Guidance for working on `huglets/hf-docs-huglet`.

## Stable contract

Keep the public API compatible with `examples/huglet-docs`: one required positional `docs_path`, then the question; generated Python module `hf_docs_huglet`; and a response with `response: str` plus `related_documents: list[{path, url}]`. The documentation path is runtime configuration and must continue to re-jail `tools.fs_read.root` before assembly.

Keep the huglet read-only and offline. Do not grant shell, write, network, memory, delegation, MCP, or another agent to improve retrieval. The daily documentation dump is the only source of truth.

The dump has generated hierarchical `AI_INDEX.md` files. They may guide retrieval but must never be cited as authoritative sources. Do not hardcode today's topic list, filenames, summaries, or dump timestamp into the prompt. Optimize methods that transfer to future light and full dumps.

## Research discipline

The research harness lives under `research/`. Dataset snapshots are immutable and content-hashed. Never edit a snapshot; create a new named snapshot when generation changes. Use only `train.jsonl` while designing a variant. Do not inspect held-out test answers or judge reasoning to tune a variant. Run the test split after the variant is fixed.

Every evaluation must use an explicit `--variant` label and store its JSON result. Candidate cost excludes judge cost in optimization charts, while the result retains both. Documentation fingerprint drift is recorded because the dumps evolve; use `--require-docs-match` for an exact historical comparison.

Treat end-to-end evaluation as the acceptance gate. At minimum, build and install the generated Python wheel, run the fixed test split, and regenerate the charts. Rust unit tests are optional for prompt-only changes. Run the Python harness tests when changing dataset, evaluator, storage, or reporting code.

Do not change the dataset, judge prompt, scoring thresholds, or report formulas in the same variant comparison as a huglet optimization. Such a change creates a new benchmark version and invalidates direct comparisons with earlier results.

## Commands

From this folder, after the setup in `research/README.md`:

```bash
cargo run -p huggr-toolkit --bin huggr -- build . --surface python --release
research/.venv/bin/hf-docs-research evaluate --docs ../../hf-dump-light --dataset research/datasets/hf-docs-v1 --split test --variant <name>
research/.venv/bin/hf-docs-research report
research/.venv/bin/pytest research/tests
```

Before committing a variant, record the prompt, manifest, or contract change in the commit and keep the corresponding result JSON and regenerated charts with it.
