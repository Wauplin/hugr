# huglet-docs

`huglet-docs` is the checked-in reference documentation-retrieval agent crate. `huggr.toml` and `SYSTEM.md` live at the crate root beside the tiny Rust module that owns the typed `DocsResponse` contract. `huggr-toolkit` does not depend on `huglet-docs`; generic `huggr run` works by compiling a cached dev shim that links this crate, matching the built-binary path.

## Usage

```bash
export HF_TOKEN=hf_...
cargo run -p huggr-toolkit --bin huggr -- run examples/huglet-docs ./docs "What is the narrow-waist rule?" | jq
```

To build the standalone artifact:

```bash
cargo run -p huggr-toolkit --bin huggr -- build examples/huglet-docs --release
./examples/huglet-docs/dist/huglet-docs-cli/target/release/huglet-docs ./docs "What is the narrow-waist rule?"
```

The first runtime argument, `docs_path`, is declared in `huggr.toml` under `[runtime.args.docs_path]` and patches `tools.fs_read.root` for that invocation. Relative paths are resolved from the caller's current directory, so the same built binary can be run against a different docs folder each time.

The output is the standard Huggr `Answer` JSON:

```json
{
  "status": "success",
  "response": {
    "response": "...",
    "related_documents": [{ "path": "README.md", "url": "https://github.com/Wauplin/huggr/blob/main/docs/README.md" }]
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

The response contract is declared in Rust, not in `huggr.toml`:

```rust
pub const RESPONSE_RUST_TYPE: &str = "huglet_docs::DocsResponse";
```

Because the agent folder is also the Rust crate, `huggr run` and `huggr build` infer the crate from the current `Cargo.toml`, read `RESPONSE_RUST_TYPE`, link the crate into the generated shim, derive provider JSON Schema from the Rust type, ask the model provider for that structured output, and cast the final JSON with serde before emitting the standard Huggr `Answer`. Huggr itself keeps one universal wire contract: `Answer.response` is a structured object.

## Runtime Args

```toml
[tools.fs_read]
root = [{ name = "docs", path = "." }]

[runtime.args.docs_path]
target = "tools.fs_read.root"
positional = true
required = true
env = "HUGGR_DOCS_PATH"
help = "Folder containing the documentation to search."
```

The toolkit uses that block to generate the CLI argument and the MCP `ask` schema. For MCP, `docs_path` is an `ask` argument, so one long-running server can answer against different docs folders on different calls.

The retrieval jail accepts any documentation folder, but the example's deterministic answer hook maps cited paths to the public Huggr repository under `docs/`. Change `HUGGR_DOCS_BASE` or the hook when adapting this crate to another corpus.

## Tooling Model

The manifest grants the toolkit's read-only `fs_read` library. At runtime, `docs_path` scopes that grant to one canonicalized folder and registers `fs_list`, `fs_search`, `fs_grep`, `fs_glob`, `fs_read`, `fs_read_range`, `fs_read_many`, and `fs_outline`.

Each filesystem tool rejects absolute paths, parent-directory traversal, and symlink escapes outside the runtime docs root. There is no shell access, no write/edit tool, and no HTTP tool.
