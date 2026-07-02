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
  "answer": "By default, you'll be watching all the organizations you are a member of, and will be notified of any new activity on those.",
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

Use `--pretty` to pretty-print the JSON and `--model <id>` to override the model for a single run.

The final JSON object is the only stdout output. Operational logs, model/tool lifecycle events, streamed model chunks, and errors are written to stderr so stdout remains safe to pipe into `jq`.

## Configuration

All environment variables are crate-specific and independent from `hugr-cli`'s `HUGR_*` configuration:

| Variable | Default | Notes |
| --- | --- | --- |
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

The system prompt instructs the model to use only the docs tools and to finish with a JSON object containing `answer` and `related_documents`. If the docs do not contain enough evidence, it must answer: `It is not possible to find an answer in the docs.`

The CLI always emits valid JSON even if the final model text is imperfect: it parses fenced or raw JSON when possible, otherwise wraps the text as `answer`; related documents are sanitized, limited to non-index documents actually read during the run, and fall back to the full non-index read set when needed.

## Troubleshooting

If the run fails before a final answer, the CLI reports the recorded terminal model/tool error. Common causes are an invalid `HUGR_DOCS_API_KEY`, a `HUGR_DOCS_BASE_URL` that is not OpenAI-compatible, or a model that does not support function/tool calling.
