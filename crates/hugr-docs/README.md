# hugr-docs

`hugr-docs` is the checked-in reference documentation-retrieval agent. It is a Hugr definition folder at `crates/hugr-docs/definition` plus a tiny Rust crate that owns and registers the typed `DocsResponse` contract. The crate consumes `hugr-toolkit`'s shared surface; `hugr-toolkit` does not depend on `hugr-docs`.

## Usage

```bash
export HUGR_DOCS_API_KEY=hf_...
cargo run -p hugr-docs -- ./docs "What is the narrow-waist rule?" | jq
```

To build the standalone artifact:

```bash
cargo run -p hugr-toolkit --bin hugr -- build crates/hugr-docs/definition --release
./crates/hugr-docs/definition/dist/hugr-docs-cli/target/release/hugr-docs ./docs "What is the narrow-waist rule?"
```

The first runtime argument, `docs_path`, is declared in `definition/hugr.toml` under `[runtime.args.docs_path]` and patches `tools.fs_read.root` for that invocation. Relative paths are resolved from the caller's current directory, so the same built binary can be run against a different docs folder each time.

The output is the standard Hugr `Answer` JSON:

```json
{
  "status": "success",
  "response": {
    "response": "...",
    "related_documents": ["docs/ARCHITECTURE.md"]
  },
  "trace_id": "1e4f7d0a9b2c3d44",
  "blobs": [],
  "metadata": {
    "duration_ms": 1234,
    "cost_micro_usd": 1300,
    "tokens_in": 1000,
    "tokens_out": 200,
    "model_calls": 2,
    "tool_calls": 3
  },
  "extra": null
}
```

The definition declares `[response] rust_type = "hugr_docs::DocsResponse"` plus the crate path needed by `hugr build`, so the generated binary links the agent crate, derives provider JSON Schema from the Rust type, asks the model provider for that structured output, and casts the final JSON with serde before emitting the standard Hugr `Answer`. Hugr itself keeps one universal wire contract: `Answer.response` is a structured object.

## Runtime Args

```toml
[tools.fs_read]
root = "."

[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
env = "HUGR_DOCS_PATH"
help = "Folder containing the documentation to search."

[response]
rust_type = "hugr_docs::DocsResponse"
crate_path = ".."
crate_package = "hugr-docs"
schema_name = "hugr_docs_response"
max_attempts = 3
```

The toolkit uses that block to generate the CLI argument and the MCP `ask` schema. For MCP, `docs_path` is an `ask` argument, so one long-running server can answer against different docs folders on different calls.

## Tooling Model

The definition grants the toolkit's read-only `fs_read` library. At runtime, `docs_path` scopes that grant to one canonicalized folder and registers `fs_list`, `fs_search`, `fs_read`, `fs_read_range`, `fs_read_many`, and `fs_outline`.

Each filesystem tool rejects absolute paths, parent-directory traversal, and symlink escapes outside the runtime docs root. There is no shell access, no write/edit tool, and no HTTP tool.
