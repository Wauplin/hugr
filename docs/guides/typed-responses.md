# Typed responses and answer hooks

This guide explains how a Huggr agent's response contract works. `RESPONSE_RUST_TYPE` turns a Rust struct into the provider's structured-output schema. The optional `MODEL_RESPONSE_RUST_TYPE` lets the model fill a simpler schema than the one users receive. `answer_hooks()` bridges the two with deterministic post-processing.

The worked example is the checked-in reference agent `examples/huglet-docs`. It enriches model-cited document paths into public Hugging Face documentation URLs.

Prerequisite: [Build your first agent](../tutorials/first-agent.md). For the design background, see [language surfaces](../reference/agents.md#language-surfaces).

## How the contract is discovered

An agent folder is also a Rust crate, and `huggr build` (and typed `huggr run`, which uses the same shim) reads three things straight out of `src/lib.rs`:

- `pub const RESPONSE_RUST_TYPE: &str = "crate_name::TypeName";` is **required**. The build fails with an explicit error if it is missing, and the value must look like `crate_name::TypeName`. The part before `::` names the dependency linked by the generated shim. If your Cargo package name differs (`huglet-docs` vs `huglet_docs`), the shim handles the rename.
- `pub const MODEL_RESPONSE_RUST_TYPE: &str = ...;` is **optional**. When present and different from `RESPONSE_RUST_TYPE`, its schema is what the provider produces. The public type remains visible to callers in `Answer.response` and in `--config` output. When absent, one type plays both roles.
- `pub fn answer_hooks() -> Vec<AnswerHook>` is **optional**. If the source contains such a function, the generated shim registers the hooks and runs them on every finished `Answer`.

There is no registration ceremony: export the const(s) and the function, and the build wires everything. Under the hood the shim calls `ResponseContract::from_type::<Model>(...)` (plus `.with_public_type::<Public>()` when the two differ) and `.with_answer_hooks(...)`; you never write that code yourself.

## The worked example: huglet-docs

`examples/huglet-docs` answers questions from a read-only docs folder and cites its sources. The design problem: we want users to get *URLs*, but asking the model to construct `https://huggingface.co/docs/...` URLs invites hallucination. The fix is a split contract; the model cites bare paths, and a hook derives the URLs deterministically.

### Two response types

From `examples/huglet-docs/src/lib.rs`:

```rust
pub const RESPONSE_RUST_TYPE: &str = "huglet_docs::DocsResponse";
pub const MODEL_RESPONSE_RUST_TYPE: &str = "huglet_docs::DocsModelResponse";

/// Public response payload returned by the docs agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsResponse {
    pub response: String,
    pub related_documents: Vec<Document>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub path: String,
    pub url: String,
}

/// Model-facing response payload. The model cites paths only; the final answer
/// hook deterministically derives URLs after the response casts successfully.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DocsModelResponse {
    pub response: String,
    pub related_documents: Vec<String>,
}
```

The model sees the schema of `DocsModelResponse`, where `related_documents` is an array of strings. Users, `--config`, and downstream callers see `DocsResponse`, where each document is a `{path, url}` object. `#[serde(deny_unknown_fields)]` on `DocsModelResponse` keeps the model-output cast strict: extra keys cause a cast failure instead of becoming silent baggage. The public type supplies the generated surface schema; Huggr does not cast the hook's post-processed value a second time.

### The bridge: answer_hooks()

A hook is a named function that mutates the finished `Answer` in place, after the model output has been cast against the model type:

```rust
pub fn answer_hooks() -> Vec<AnswerHook> {
    vec![AnswerHook::new("huglet_docs::document_urls", |answer| {
        if answer.status != STATUS_SUCCESS {
            return;
        }
        let Some(related) = answer
            .response
            .get_mut("related_documents")
            .and_then(Value::as_array_mut)
        else {
            return;
        };
        for document in related {
            if let Some(path) = document.as_str().map(str::to_string) {
                let url = document_url(&path);
                *document = json!({ "path": path, "url": url });
            }
            // (the real code also tolerates already-object entries)
        }
    })]
}
```

Three habits worth copying:

- **Bail early on non-success.** Error answers carry `{"error": ...}` in `response`; a hook should leave them alone.
- **Be defensive about shape.** Hooks work on `serde_json::Value`, so match with `get_mut`/`as_array_mut` and skip quietly rather than unwrap; the huglet-docs hook even handles entries that are already objects, making it idempotent.
- **Give the hook a namespaced name** (`"huglet_docs::document_urls"`): the name identifies the hook in diagnostics.

Hooks must be deterministic pure transformations, such as string processing or lookups against shipped data, with no IO or clock access. They run after trace, blob, and scratch finalization and are not recorded in the trace. Use a hook for work such as deriving documentation URLs; use a model tool for network calls.

The hook is unit-testable like any function; build an `Answer`, apply `answer_hooks()`, assert on `answer.response` (see the test at the bottom of `examples/huglet-docs/src/lib.rs`).

### Run it

From the repo root:

```bash
export HF_TOKEN=hf_...
huggr run examples/huglet-docs ./docs "What is the narrow-waist rule?"
```

(`./docs` fills the agent's required positional runtime argument `docs_path`, declared under `[runtime.args.docs_path]` in its `huggr.toml`; it re-jails `fs_read` to that folder per invocation.) The answer's `response.related_documents` comes back as `[{"path": "...", "url": "https://huggingface.co/docs/..."}]`; paths chosen by the model, URLs stamped by the hook.

## Growing your own contract

Take the weather agent from [Build your first agent](../tutorials/first-agent.md) and evolve it the same way:

1. Widen `Response` in `src/lib.rs` with the fields you want callers to rely on (say `temperature_c: f64`, `conditions: String`). That alone changes the provider schema on the next run; no manifest edit, no rebuild command beyond `huggr run`/`huggr build` as usual.
2. If a field is mechanical (units conversion, formatting, canonical links), move it out of the model's schema: declare a leaner model type, point `MODEL_RESPONSE_RUST_TYPE` at it, and compute the field in a hook.
3. Keep `RESPONSE_RUST_TYPE` naming the public type. That is the contract received by every downstream consumer, including the CLI JSON, MCP, and agent-as-tool callers.

The rule of thumb: **the model fills in what only the model knows; hooks fill in what code can derive.** Everything derivable is one less thing the model can get wrong.

## Next

Continue with [Build a Chrome extension](../tutorials/chrome-extension.md), read `examples/huglet-docs/README.md` for runtime arguments and MCP serving, or see [agents as tools](../reference/agents.md#agents-as-tools) to compose the binary into a larger agent.
