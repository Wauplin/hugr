# Build and evaluate a docs Q&A dataset

This tutorial composes three Huggr specialists into a pipeline: `huglet-datasmith` mines a documentation folder into grounded question/answer pairs, `hf-librarian` publishes them as a Hugging Face dataset, and an eval script scores the `huglet-docs` agent against the result. Run against Huggr's own `docs/`, it produces, and then uses, an evaluation set for the reference docs agent. The finished code is checked in at `examples/huglet-datasmith` and `examples/hf-librarian`.

A generic agent with a shell and your `HF_TOKEN` could do this job. The point of the pipeline is what each specialist *cannot* do: the datasmith can read only the docs folder it is pointed at and must return a typed dataset; the librarian's entire tool surface is three Python functions bound to one dataset repo, so the Hub token in your environment never becomes a general-purpose capability. Every ask leaves a replayable trace with itemized cost.

The tutorial is self-contained: the next section covers the Huggr concepts it uses, and every command and output below comes from a real run. For depth on any topic, follow the links into the [reference documentation](../README.md) and the [guides](../guides/README.md).

## What you need to know about Huggr

**A huglet is a folder that becomes a binary.** An agent is a small crate: a `huggr.toml` manifest (model tiers, tool grants, limits), a `SYSTEM.md` prompt, and optionally a typed Rust response contract in `src/lib.rs`. `huggr run <dir> "<question>"` runs it in place; `huggr build <dir>` compiles it into one standalone binary. See [Build your first agent](first-agent.md) and [the overview](../concepts/overview.md).

**Ask in, Answer out, and turn errors are answers.** Completed turns return an `Answer` with a `status`, a JSON `response`, a `trace_id`, and mandatory `metadata` (cost in micro-USD, tokens, model/tool call counts, duration). A traced turn failure is a `status: "error"` answer with the same metadata; configuration and infrastructure failures can fail before an answer exists. The CLI ask path converts those failures to error answers and exits 0. See [agents](../reference/agents.md).

**Every completed turn leaves an immutable trace.** The full session (every model call, tool call, and result) persists as a trace file under `~/.huggr/<agent>/traces/`. Passing `trace_id=` to a later ask resumes that conversation (the trace is re-folded, nothing re-runs); asking the same parent twice forks it. Traces replay deterministically: `huggr replay --step` reconstructs a run event by event. See [Inspect, replay, and verify traces](../guides/inspect-traces.md).

**Tools are granted, not discovered.** An agent can only invoke what its definition registers, such as `[tools.fs_read]` jailed to a declared root or a Python callable supplied by the host. This agent does not grant the optional shell. See [the capability reference](../reference/capabilities.md) and [security model](../concepts/security.md).

**Typed response contracts.** A Rust struct exported as `RESPONSE_RUST_TYPE` becomes the provider's structured-output schema, and the final model JSON is cast into it before it reaches you. Downstream code gets dataclasses, not string parsing. See [Define typed responses and answer hooks](../guides/typed-responses.md).

**One runtime, several surfaces.** The same built agent is a CLI binary, an MCP server (`--mcp-serve`), or a typed Python wheel (`--surface python`). Separately, the `huggr-agents` Python package lets you define new agents directly in Python, with tools as annotated functions and config as data, on the same Rust runtime. See [Package an agent for Python](../guides/package-agent-for-python.md) and [Define an agent in Python](python-agent.md).

## The datasmith: a synthetic-data specialist in Rust

`examples/huglet-datasmith/huggr.toml` declares one model tier and grants exactly one tool, jailed to a folder chosen at run time:

```toml
[agent]
name = "huglet-datasmith"
version = "0.1.0"
description = "Mines a documentation folder and synthesizes grounded Q&A evaluation pairs."

[models]
default = "powerful"

[tools.fs_read]
root = "."

[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
env = "HUGGR_DATASMITH_DOCS"
help = "Folder containing the documentation to mine for Q&A pairs."
```

The runtime argument is the same pattern `huglet-docs` uses: the first positional argument patches `tools.fs_read.root`, so one built agent works on any docs folder while staying read-only inside it.

`SYSTEM.md` is where the domain expertise lives. It is a *generation methodology*, not a persona:

```markdown
You are Huggr DataSmith, a synthetic-dataset specialist. Your one job is to mine the
documentation folder exposed through your filesystem tools and produce grounded
question/answer pairs for evaluating documentation assistants.

Method: first list the folder and skim enough files to map the topics, then write the
pairs. For every pair: the question must sound like something a real user would ask
(no "according to section 3..." phrasing), the expected answer must be fully supported
by one source file you actually read, and `source_path` must be that file's path
relative to the docs root. [...] Never invent facts that are not in the docs; skip a
topic rather than guess.
```

The response contract in `src/lib.rs` is what makes the output machine-usable downstream:

```rust
pub const RESPONSE_RUST_TYPE: &str = "huglet_datasmith::QaDataset";

pub struct QaDataset {
    pub items: Vec<QaItem>,
    pub coverage: String,
}

pub struct QaItem {
    pub question: String,
    pub expected_answer: String,
    pub source_path: String,
    pub difficulty: String,
}
```

`source_path` forces grounding: every pair must cite the file that supports it, which the system prompt reinforces by requiring the model to skim files before writing pairs and to skip topics rather than guess. `difficulty` is an open string label, as core conventions require; nothing branches on it.

## Set up the environment and build the wheels

The pipeline calls its Rust agents in-process, not over subprocesses: `huggr build --surface python` wraps a built agent into a maturin wheel exposing a strictly-typed `ask()` ([Package an agent for Python](../guides/package-agent-for-python.md)). You need Rust, [uv](https://docs.astral.sh/uv/), [maturin](https://maturin.rs) (`uv tool install maturin`), and the `huggr` CLI (`cargo install --path crates/huggr-toolkit`).

From `examples/hf-librarian/`, create the environment and install the PyPI dependencies:

```bash
uv venv --python 3.12
uv pip install -r requirements.txt
```

Then build the three Huggr packages as wheels (the `huggr-agents` runtime package plus the two agents) and install them. Only these come from local builds; everything else is PyPI:

```bash
export HF_TOKEN=hf_...                               # key for the default Hugging Face provider
(cd ../../bindings/python && maturin build --release)
huggr build ../huglet-datasmith --surface python --release
huggr build ../huglet-docs --surface python --release
uv pip install ../../crates/huggr-python/target/wheels/*.whl \
               ../huglet-datasmith/dist/huglet-datasmith-python/target/wheels/*.whl \
               ../huglet-docs/dist/huglet-docs-python/target/wheels/*.whl
```

After installing, calling an agent is one typed function call with no subprocess and no JSON parsing. `Answer.response` is a generated `QaDataset` dataclass mirroring the Rust contract; Rust already cast the model output, so Python only deserializes valid JSON into typed objects:

```python
import huglet_datasmith

answer = huglet_datasmith.ask("../../docs", "Generate 10 question/answer pairs.")
if answer.ok:
    first = answer.response.items[0]         # a typed QaItem
    print(first.question, "→", first.source_path)
else:
    print("error:", answer.error)             # traced turn errors are answers
```

## The librarian: a jail made of closures

The publishing side lives in `examples/hf-librarian/pipeline.py`, defined entirely on the [Python surface](python-agent.md). The repo id and staged file are module-level constants the host fixed. Each tool is an annotated function, and `@huggr.tool` infers the advertised schema from the signature and docstring, so the model never chooses *where* anything goes:

```python
import huggr_agents as huggr
from huggingface_hub import HfApi

api = HfApi()
REPO_ID = f"{api.whoami()['name']}/huglet-docs-qa"


@huggr.tool
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


@huggr.tool
def upload_readme(content: str) -> str:
    """Upload the dataset card (markdown with YAML front matter) as the repo's README.md."""
    if not content.startswith("---"):
        return "rejected: the card must start with YAML front matter (---)"
    api.upload_file(
        path_or_fileobj=content.encode(), path_in_repo="README.md", repo_id=REPO_ID, repo_type="dataset"
    )
    return f"uploaded README.md to {REPO_ID}"


@huggr.tool
def publish_data() -> str:
    """Upload the staged JSONL data file as data/qa.jsonl of the repo."""
    api.upload_file(
        path_or_fileobj=STAGED, path_in_repo="data/qa.jsonl", repo_id=REPO_ID, repo_type="dataset"
    )
    return f"uploaded data/qa.jsonl to {REPO_ID}"
```

This is the Python-surface trust model used deliberately: tool callables are trusted host code, so the jail is the function body itself. `dataset_summary` gives the model statistics and three sample rows instead of the whole file, which is enough to write an honest dataset card without paying to stream every pair through context. `publish_data` takes no arguments at all; the staged JSONL and its destination are fixed.

The agent itself is data: a specialist system prompt, one model tier, the three tools, and a `response_schema` pinning the final answer to `{notes}`:

```python
librarian = huggr.Agent(
    name="hf-librarian",
    description="Publishes one staged dataset to one Hugging Face dataset repo.",
    system=(
        "You are hf-librarian, a Hugging Face Hub publishing specialist. Call `dataset_summary` first "
        "and ground everything you write in it. Then write a proper dataset card and upload it with "
        "`upload_readme`: YAML front matter (license, task_categories, tags), a short summary, a "
        "field-by-field schema description, how the data was generated, the intended use, and honest "
        "limitations. Finally call `publish_data`. Respond only with the structured JSON response."
    ),
    models={
        "default": "powerful",
    },
    tools=[dataset_summary, upload_readme, publish_data],
    response_schema={
        "type": "object",
        "properties": {"notes": {"type": "string"}},
        "required": ["notes"],
        "additionalProperties": False,
    },
)
```

The orchestration is plain Python: generate, stage, and publish, with the deterministic parts (staging the JSONL and creating the repo) done by the host, not the model:

```python
generated = huglet_datasmith.ask(str(DOCS), f"Generate {COUNT} question/answer pairs.")
dataset = generated.response
STAGED.write_text("".join(json.dumps(asdict(item)) + "\n" for item in dataset.items))

api.create_repo(REPO_ID, repo_type="dataset", exist_ok=True, private=True)
published = librarian.ask(f"Publish the staged docs-QA dataset to {REPO_ID}.")

cost = generated.metadata.cost_micro_usd + published.metadata.cost_micro_usd
```

The repo is created private by default. The synthesized Q&A includes source file names, so a run against internal docs would otherwise publish them publicly; set `private=False` deliberately when the dataset is meant to be public.

## Run the pipeline

```bash
hf auth login                  # Hub credentials for the librarian
.venv/bin/python pipeline.py
```

The two halves run in one process: the datasmith through its wheel, the librarian on the embedded runtime. The run ends with the pipeline's accounting, folded from both agents' `AnswerMeta`:

```text
[1/2] datasmith: mining /home/you/huggr/docs for 10 Q&A pairs...
      10 pairs, coverage: The pairs span the core vision, agent definition, runtime architecture,
      security model, response contracts, agent composition, and trace debugging.
[2/2] hf-librarian: publishing to <you>/huglet-docs-qa...
      Successfully published the Huggr Docs QA dataset to <you>/huglet-docs-qa. The process included
      generating a dataset summary, uploading a comprehensive README with YAML front matter
      (including 'huggr' and 'synthetic' tags), and publishing the JSONL data file.

cost: 90799 µUSD, traces: datasmith=fa808c069b1500e6 librarian=9eb44174ebe8188e
dataset: https://huggingface.co/datasets/<you>/huglet-docs-qa
```

## Evaluate huglet-docs against the dataset

`eval.py` downloads `data/qa.jsonl` back from the Hub, has `huglet_docs.ask(...)` answer every question, and grades each answer with a third specialist, a tool-free `qa-judge` agent whose `response_schema` pins its verdict to `{correct, reasoning}`:

```python
judge = huggr.Agent(
    name="qa-judge",
    description="Grades a candidate answer against the expected one.",
    system=(
        "You grade documentation answers. Given a question, the expected answer, and a candidate "
        "answer, decide whether the candidate conveys the expected facts. Judge meaning, not "
        "wording; extra correct detail is fine."
    ),
    models={
        "default": "powerful",
    },
    response_schema={
        "type": "object",
        "properties": {"correct": {"type": "boolean"}, "reasoning": {"type": "string"}},
        "required": ["correct", "reasoning"],
        "additionalProperties": False,
    },
)
```

The loop composes the two: the docs agent answers from the docs jail, the judge sees only `{question, expected, candidate}`:

```python
data = hf_hub_download(REPO_ID, "data/qa.jsonl", repo_type="dataset")
items = [json.loads(line) for line in Path(data).read_text().splitlines()]

for item in items:
    answered = huglet_docs.ask(str(DOCS), item["question"])
    candidate = answered.response.response      # DocsResponse.response, typed

    graded = judge.ask(json.dumps(
        {"question": item["question"], "expected": item["expected_answer"], "candidate": candidate}
    ))
    ok = graded.response["correct"]
```

```bash
.venv/bin/python eval.py
```

```text
evaluating huglet-docs on 10 questions from <you>/huglet-docs-qa

 1. PASS [basic] What is a huglet composed of?
 2. PASS [basic] Which command is used to create a new agent crate folder?
 3. PASS [intermediate] How does Huggr ensure that resuming a conversation is immediate and doesn't require new model calls?
 4. PASS [advanced] What is the 'narrow-waist rule' in the context of the Huggr brain and host contract?
 5. PASS [intermediate] How does Huggr prevent path traversal attacks in the `fs_read` tool?
 6. PASS [basic] What is the purpose of the `RESPONSE_RUST_TYPE` constant in an agent's `src/lib.rs`?
 7. PASS [intermediate] When granting one Huggr agent to another as a tool, how are large files passed between them without copying bytes?
 8. PASS [advanced] How is cost attributed when an orchestrator agent calls a child huglet?
 9. PASS [intermediate] What is the difference between `huggr replay` and `huggr verify`?
...

score: 10/10, eval cost: 122045 µUSD
```

A failing row prints the expected answer, the candidate, and the judge's reasoning. This is the starting point for fixing either the docs agent or the docs themselves.

## Inspect the runs: `huggr traces` and `huggr stats`

Every ask above persisted a trace, and the `huggr` CLI reads them straight from an agent's folder with no code or running process. `huggr traces` lists the store as a lineage: one line per ask with its id, outcome status, feedback count, and question. This is where you find the trace id to resume, replay, or attach feedback to:

```text
$ huggr traces examples/huglet-datasmith
• fa808c069b1500e6 [success] feedback=0 Generate 10 question/answer pairs covering the whole docume…
```

The eval left ten sibling traces on `huglet-docs`, one per question, and each is independently resumable:

```text
$ huggr traces examples/huglet-docs
• 08b6926930d79693 [success] feedback=0 How is cost attributed when an orchestrator agent calls a c…
• 0df512660261c430 [success] feedback=0 What is the purpose of the `RESPONSE_RUST_TYPE` constant in…
• 34cd7a97bf39c112 [success] feedback=0 What is the difference between `huggr replay` and `huggr veri…
...
```

`huggr stats` folds every trace in the store into an aggregate: ask and feedback counts, cost (split into the agent's *own* spend vs. spend *delegated* to child agents), token totals, latency percentiles, and per-model and per-tool breakdowns:

```text
$ huggr stats examples/huglet-datasmith
asks: 1  feedback: 0
cost: total=$0.09 own=$0.09 delegated=$0.00
tokens: in=82598 out=2170  calls: models=5 tools=4
duration_ms: mean=4883 median=4883 p95=4883

models:
  powerful calls=5 tokens_in=82598 tokens_out=2170 cost=$0.09

tools:
  fs_list calls=1 errors=0 total_latency_ms=1 mean_latency_ms=1
  fs_read_many calls=1 errors=0 total_latency_ms=1 mean_latency_ms=1
  scratch_read calls=1 errors=0 total_latency_ms=0 mean_latency_ms=0
  scratch_write calls=1 errors=0 total_latency_ms=1 mean_latency_ms=1
```

This one screen answers the operational questions a pipeline owner actually has. Where does the money go? With 82.6k input tokens against 2.2k output, the datasmith's cost is reading docs, so trimming what it reads (or using a cheaper tier for skimming) is the lever. Is the agent behaving? The tool rows show it listed the folder once, bulk-read files once, and used its scratchpad, with no errors or thrashing. And the same view over `huglet-docs` after the eval shows the per-question shape:

```text
$ huggr stats examples/huglet-docs
asks: 10  feedback: 0
cost: total=$0.10 own=$0.10 delegated=$0.00
tokens: in=99752 out=2063  calls: models=37 tools=28
duration_ms: mean=1886 median=1782 p95=3992

models:
  docs calls=37 tokens_in=99752 tokens_out=2063 cost=$0.10

tools:
  fs_list calls=2 errors=0 total_latency_ms=2 mean_latency_ms=1
  fs_read calls=7 errors=0 total_latency_ms=3 mean_latency_ms=0
  fs_search calls=19 errors=0 total_latency_ms=35 mean_latency_ms=1
```

Ten asks, ~10.3¢, p95 under 4 seconds, and `fs_search`-heavy tool use show that the docs agent searches more than it reads, which is exactly what you want from a Q&A specialist. Because stats fold from traces, these numbers accumulate across runs: re-run the eval after changing `SYSTEM.md` and the deltas in cost, latency, and tool mix are your regression report. The `feedback=0` column is the reminder that verdicts can be attached back to traces (`agent.feedback(trace_id, ...)`), which the offline `examples/huglet-insights` agent then mines for improvement suggestions.

To see *inside* a single run rather than the aggregate, replay it deterministically to inspect every file the datasmith read before writing each pair, event by event:

```bash
huggr replay examples/huglet-datasmith fa808c069b1500e6 --step
```

The Python-defined agents (`hf-librarian`, `qa-judge`) have no manifest folder for the CLI to point at, but the same data is available in-process: `librarian.traces()` and `librarian.stats()` return the identical listings and aggregates ([Define an agent in Python](python-agent.md)).

## Next

Point the datasmith at your own project's docs and publish an eval set for *your* assistant, or extend the loop: file each eval verdict as feedback on the `huglet-docs` traces and let `examples/huglet-insights` mine them for prompt improvements. If a batch of pairs is weak, resume the datasmith trace (`huglet_datasmith.ask(..., trace_id=...)`) to regenerate. The new ask is a sibling trace, and the original stays immutable.
