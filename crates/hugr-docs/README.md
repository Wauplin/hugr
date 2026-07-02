# hugr-docs

`hugr-docs` is a specialized Hugr host for documentation retrieval. It demonstrates the same sans-IO brain running behind a very different product surface from `hugr-cli`: one folder in, one question in, one JSON answer out.

The crate does not depend on `hugr-cli`. It uses `hugr-core` for the reducer, `hugr-host` for the driver loop and capability/model traits, and `hugr-providers` for the OpenAI-compatible streaming adapter.

## Usage

```bash
export HUGR_DOCS_API_KEY=hf_...
cargo run -p hugr-docs -- ./archive-light-2026-07-01 "Which repositories do I watch by default?" | jq
```

Output shape:

```json
{
  "status": "success",
  "message": "By default, you'll be watching all the organizations you are a member of, and will be notified of any new activity on those.",
  "related_documents": ["hub/notifications.md"],
  "metadata": {
    "model": "google/gemma-4-31B-it:cerebras",
    "endpoint": "https://router.huggingface.co/v1",
    "elapsed_ms": 1234,
    "tokens_in": 1000,
    "tokens_out": 200,
    "estimated_cost_micro_usd": 1300,
    "input_usd_per_m_tokens": 1.0,
    "output_usd_per_m_tokens": 1.5,
    "model_calls": 2,
    "tool_calls": 3,
    "read_documents": 1,
    "read_indexes": 1
  }
}
```

`status` is a string enum with three values, all still emitted as a single JSON object on stdout with exit code `0`:

- `"success"` — the model produced an answer.
- `"off_topic"` — the docs did not contain enough evidence; `message` is the `It is not possible to find an answer in the docs.` phrase.
- `"error"` — an error stopped the run before a final answer (bad API key, missing docs root, the model never returned a final answer, a provider/transport failure, …); `message` is the error text.

Use `--pretty` to pretty-print the JSON and `--model <id>` to override the model for a single run.

The final JSON object is the only stdout output and the CLI always exits `0`, so stdout remains safe to pipe into `jq` and the Python binding never raises for a run failure. Operational logs, model/tool lifecycle events, streamed model chunks, and errors are written to stderr.

## Python binding

The crate also builds a Python extension module with one method, `hugr_docs.answer(question, docs_path=None, api_key=None, base_url=None, model=None, input_usd_per_m_tokens=None, output_usd_per_m_tokens=None)`, returning a Python `dict` with the same `status`, `message`, `related_documents`, and `metadata` fields emitted by the CLI. The binding never raises for a run failure — config errors, missing docs roots, and model/transport failures all come back as `{"status": "error", "message": "<error>", ...}` so callers can branch on `result["status"]` without a `try`/`except`.

Build or install it with maturin from this directory:

```bash
cd crates/hugr-docs
maturin develop --features python
```

Then call it from Python:

```python
import hugr_docs

result = hugr_docs.answer(
    "Which repositories do I watch by default?",
    docs_path="./archive-light-2026-07-01",
    api_key="hf_...",
    base_url="https://router.huggingface.co/v1",
    model="google/gemma-4-31B-it:cerebras",
    input_usd_per_m_tokens=1.0,
    output_usd_per_m_tokens=1.5,
)
if result["status"] == "success":
    print(result["message"])
else:
    print(result["status"], ":", result["message"])
print(result["metadata"])
```

Each optional argument falls back independently: `docs_path` uses `HUGR_DOCS_PATH`, `api_key` uses `HUGR_DOCS_API_KEY`, `base_url` uses `HUGR_DOCS_BASE_URL` then the default endpoint, `model` uses `HUGR_DOCS_MODEL` then the default model, and pricing uses the matching env var then the built-in default. This means callers can run fully from explicit Python arguments, fully from environment variables, or mix the two.

## Configuration

All environment variables are crate-specific and independent from `hugr-cli`'s `HUGR_*` configuration:

| Variable | Default | Notes |
| --- | --- | --- |
| `HUGR_DOCS_PATH` | optional | Docs root used by the Python binding when `docs_path` is omitted. |
| `HUGR_DOCS_API_KEY` | required | API key for the OpenAI-compatible endpoint. |
| `HUGR_DOCS_BASE_URL` | `https://router.huggingface.co/v1` | Endpoint root; `/chat/completions` is appended by the adapter. |
| `HUGR_DOCS_MODEL` | `google/gemma-4-31B-it:cerebras` | Default model. Must support function/tool calling. |
| `HUGR_DOCS_INPUT_USD_PER_M_TOKENS` | `1.0` | Price used for metadata cost estimation. |
| `HUGR_DOCS_OUTPUT_USD_PER_M_TOKENS` | `1.5` | Price used for metadata cost estimation. |

Sampling is intentionally fixed: temperature is always `0.0`, and max tokens are not set by the crate.

Estimated cost is reported in microUSD. With the defaults, each input token costs `1` microUSD and each output token costs `1.5` microUSD.

## Tooling model

The harness registers read-only documentation capabilities:

- `docs_list` lists files/directories under the docs root.
- `docs_search` performs case-insensitive substring search over text-like files and returns path/line/snippet matches.
- `docs_read` reads a single text document under the docs root.
- `docs_read_range` reads a 1-based inclusive line range from a single text document.
- `docs_read_many` reads multiple text documents in one call.
- `docs_read_range_many` reads multiple line ranges in one call.
- `docs_outline` returns markdown-style headings for a file or directory.

Each tool canonicalizes paths and rejects anything outside the folder passed as `docs_path`. There is no shell access, no HTTP tool, no write/edit tool, and no interactive permission mode. Because all registered tools are read-only, the host uses `AllowAll`; this is effectively safe-autonomous mode rather than the general CLI's risky yolo mode.

`AI_INDEX.md` files are treated as navigation aids. The model may use them during search, but the final `related_documents` list filters them out and counts them separately as `read_indexes`.

## Answer contract

The system prompt instructs the model to use only the docs tools, decompose compound questions into facets, gather evidence for every facet, and finish with a JSON object containing `answer` and `related_documents`. If the docs do not contain enough evidence, it must answer: `It is not possible to find an answer in the docs.`

The CLI always emits a single valid JSON object with `status`, `message`, `related_documents`, and `metadata`, and always exits `0`. `status` is `"success"` only when the model produced a real answer; it is `"off_topic"` when the model emitted the not-found phrase and `"error"` when an error stopped the run (in which case `message` is the error text). Even when the final model text is imperfect, it parses fenced or raw JSON when possible and otherwise wraps the text as `message`; related documents are sanitized, limited to non-index documents actually read during the run, and fall back to the full non-index read set when needed.

## Troubleshooting

When an error stops a run (invalid `HUGR_DOCS_API_KEY`, a `HUGR_DOCS_BASE_URL` that is not OpenAI-compatible, a model that does not support function/tool calling, a missing docs root, or the model never returning a final answer), the CLI still prints a single JSON object with `"status": "error"` and the error text in `message`, and exits `0`. The recorded terminal model/tool error is surfaced there. Operational logs remain on stderr.
