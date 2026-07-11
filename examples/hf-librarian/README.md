# hf-librarian

An end-to-end pipeline on Hugr's Python surface, in two scripts:

- `pipeline.py` — the **hugr-datasmith** agent (called in-process through its typed Python wheel) mines this repo's `docs/` into grounded Q&A pairs, then **hf-librarian** (defined right in the script with `hugr-agents`) writes a dataset card and publishes everything to a Hugging Face dataset repo.
- `eval.py` — downloads the published dataset, has the **hugr-docs** agent answer every question, and grades each answer with a **qa-judge** agent. The output is a score for `hugr-docs` on its own documentation.

The point of using Hugr specialists instead of one generic agent: the datasmith can read only the docs folder it is pointed at, the librarian's entire tool surface is three Python functions bound to one dataset repo (your Hub credentials never become a general-purpose capability), and every ask leaves a replayable trace with itemized cost.

## Setup

You need Rust, [`uv`](https://docs.astral.sh/uv/), and [`maturin`](https://maturin.rs) (`uv tool install maturin`). Install the `hugr` CLI once: `cargo install --path ../../crates/hugr-toolkit`.

Create the environment (Python 3.12) and install the PyPI dependencies, from this folder:

```bash
uv venv --python 3.12
uv pip install -r requirements.txt
```

Build the three Hugr packages as wheels — the `hugr-agents` runtime package plus the two agents — and install them. Only these come from local builds; everything else is PyPI:

```bash
(cd ../../bindings/python && maturin build --release)
hugr build ../hugr-datasmith --surface python --release
hugr build ../hugr-docs --surface python --release
uv pip install ../../crates/hugr-python/target/wheels/*.whl \
               ../hugr-datasmith/dist/hugr-datasmith-python/target/wheels/*.whl \
               ../hugr-docs/dist/hugr-docs-python/target/wheels/*.whl
```

Each agent wheel exposes an in-process `ask(docs_path, question) -> Answer` whose `response` is the agent's typed contract (`QaDataset`, `DocsResponse`) — no subprocess, no JSON parsing.

## Run

```bash
export HUGR_API_KEY=...        # provider key for the HF router
hf auth login                  # Hub credentials (librarian uploads, eval downloads)

.venv/bin/python pipeline.py   # generate → publish to <you>/hugr-docs-qa
.venv/bin/python eval.py       # download → hugr-docs answers → judge grades
```

Both scripts print per-agent cost and trace ids; inspect any run with `hugr traces`, `hugr stats`, and `hugr replay --step` against the agent folder. The full walkthrough — including the Hugr concepts involved — is [the docs-QA pipeline guide](../../docs/guides/docs-qa-dataset-pipeline.md).
