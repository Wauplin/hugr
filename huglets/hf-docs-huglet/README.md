# hf-docs-huglet

`hf-docs-huglet` answers questions from a local Hugging Face documentation dump. It is a production use-case huglet, not a tutorial. Its read-only jail, input argument, typed response, CLI contract, MCP surface, and generated Python API match `examples/huglet-docs`; the prompt and URL mapping are specialized for Hugging Face documentation.

The usual corpus is `hf-dump-light/`, which contains the main Hub and client documentation. `hf-dump-full/` adds many library guides and package references. Both are runtime inputs and may be refreshed daily without rebuilding the huglet.

## API

The required `docs_path` runtime argument points the read jail at one extracted dump. The generated Python wheel exposes:

```python
import hf_docs_huglet

answer = hf_docs_huglet.ask("./hf-dump-light", "How do gated model downloads work?")
if answer.ok:
    print(answer.response.response)
    for document in answer.response.related_documents:
        print(document.path, document.url)
```

`answer.response` has the same shape as the generic docs huglet:

```json
{
  "response": "...",
  "related_documents": [
    {
      "path": "hub/models-gated.md",
      "url": "https://huggingface.co/docs/hub/models-gated"
    }
  ]
}
```

The model returns relative source paths. A deterministic answer hook removes generated `AI_INDEX.md` citations, normalizes an accidental leading `docs/`, and maps Markdown paths to live `https://huggingface.co/docs/...` URLs. The hook does not read the filesystem, so it remains deterministic under replay.

## Run and build

From the repository root:

```bash
export HF_TOKEN=hf_...
cargo run -p huggr-toolkit --bin huggr -- run huglets/hf-docs-huglet ./hf-dump-light "How do I download only selected files from a repository?"
cargo run -p huggr-toolkit --bin huggr -- build huglets/hf-docs-huglet --surface python --release
```

The light and full archives and extracted directories are ignored by Git. Refresh them in place and keep the top-level folder names stable:

```text
hf-dump-light.tar     -> hf-dump-light/
hf-dump-full.tar      -> hf-dump-full/
```

The prompt does not encode a fixed list of products or pages. It navigates the current dump through hierarchical `AI_INDEX.md` files, then reads authoritative pages before answering. It prefers task guides for usage questions and only enters the full dump's package references for exact API details.

## Research workflow

[`research/`](research/) generates immutable train/test datasets, evaluates installed variants through the typed Python surface, stores append-only JSON results, and draws comparison charts. Use the train split while changing the huglet and reserve the fixed test split for evaluation. The workflow is designed as the stable target for a later auto-research coding agent; that optimization loop is intentionally not part of this phase.

See [`research/README.md`](research/README.md) for setup and commands. Scoped instructions for future coding agents are in [`AGENTS.md`](AGENTS.md).

The checked-in `hf-docs-v1` baseline uses the 2026-07-17 light dump (579 Markdown files) and a 21/9 train/test split across equal basic, intermediate, and advanced groups. The initial `baseline-v0.1.0` test run completed all nine cases with 66.7% judged accuracy, 88.9% required-source recall, 236,759 µUSD candidate cost, 3.3-second p50 latency, and an exact corpus fingerprint match. These are starting measurements for optimization, not target claims.
