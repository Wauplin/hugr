# hf-librarian

An end-to-end pipeline on Huggr's Python surface, in two scripts:

- `pipeline.py`: the **huglet-datasmith** agent (called in-process through its typed Python wheel) mines this repo's `docs/` into grounded Q&A pairs, then **hf-librarian** (defined right in the script with `huggr-agents`) writes a dataset card and publishes everything to a Hugging Face dataset repo.
- `eval.py`: downloads the published dataset, has the **huglet-docs** agent answer every question, and grades each answer with a **qa-judge** agent. The output is a score for `huglet-docs` on its own documentation.

The point of using Huggr specialists instead of one generic agent: the datasmith can read only the docs folder it is pointed at, the librarian's entire tool surface is three Python functions bound to one dataset repo (your Hub credentials never become a general-purpose capability), and every ask leaves a replayable trace with itemized cost.

[The docs-QA pipeline tutorial](../../docs/tutorials/docs-qa-dataset-pipeline.md) is the full end-to-end walkthrough of this example, with the Huggr concepts involved and real outputs from a complete run.

## Setup

You need Rust, [`uv`](https://docs.astral.sh/uv/), and [`maturin`](https://maturin.rs) (`uv tool install maturin`). Install the `huggr` CLI once: `cargo install --path ../../crates/huggr-toolkit`.

Create the environment (Python 3.12) and install the PyPI dependencies, from this folder:

```bash
uv venv --python 3.12
uv pip install -r requirements.txt
```

Build the three Huggr packages as wheels (the `huggr-agents` runtime package plus the two agents) and install them. Only these come from local builds; everything else is PyPI:

```bash
(cd ../../bindings/python && maturin build --release)
huggr build ../huglet-datasmith --surface python --release
huggr build ../huglet-docs --surface python --release
uv pip install ../../crates/huggr-python/target/wheels/*.whl \
               ../huglet-datasmith/dist/huglet-datasmith-python/target/wheels/*.whl \
               ../huglet-docs/dist/huglet-docs-python/target/wheels/*.whl
```

Each agent wheel exposes in-process `ask(docs_path, question) -> Answer` and async `run(docs_path, question)` methods. The final `response` is the agent's typed contract (`QaDataset`, `DocsResponse`), with no subprocess or caller-owned JSON parsing.

## Run

```bash
export HF_TOKEN=hf_...         # key for the default Hugging Face provider
hf auth login                  # Hub credentials (librarian uploads, eval downloads)

.venv/bin/python pipeline.py   # generate → publish to <you>/huglet-docs-qa
.venv/bin/python eval.py       # download → huglet-docs answers → judge grades
```

Both scripts print per-agent cost and trace ids; inspect any run with `huggr traces`, `huggr stats`, and `huggr replay --step` against the agent folder. For the step-by-step walkthrough, see [the docs-QA pipeline tutorial](../../docs/tutorials/docs-qa-dataset-pipeline.md).
