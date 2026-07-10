# Typed responses and answer hooks

In this tutorial you'll learn how a Hugr agent's response contract really works: how `RESPONSE_RUST_TYPE` turns a Rust struct into the provider's structured-output schema, how the optional `MODEL_RESPONSE_RUST_TYPE` lets the model fill a *simpler* schema than the one your users receive, and how `answer_hooks()` bridges the two with deterministic post-processing. The worked example is the checked-in reference agent `examples/hugr-docs`, which enriches model-cited document paths into public Hugging Face documentation URLs. Prerequisite: [tutorial 1](01-first-agent-cli.md). For the design background, see [language surfaces](../../ARCHITECTURE.md#41-language-surfaces).

## How the contract is discovered

An agent folder is also a Rust crate, and `hugr build` (and typed `hugr run`, which uses the same shim) reads three things straight out of `src/lib.rs`:

- `pub const RESPONSE_RUST_TYPE: &str = "crate_name::TypeName";` — **required**. The build fails with an explicit error if it's missing, and the value must look like `crate_name::TypeName` (the part before `::` names the dependency the generated shim links; if your Cargo package name differs — `hugr-docs` vs `hugr_docs` — the shim handles the rename).
- `pub const MODEL_RESPONSE_RUST_TYPE: &str = ...;` — **optional**. When present and different from `RESPONSE_RUST_TYPE`, this type's schema is what the provider is asked to produce; the public type stays what callers see in `Answer.response` and in `--config`/`--describe` output. When absent, one type plays both roles.
- `pub fn answer_hooks() -> Vec<AnswerHook>` — **optional**. If the source contains such a function, the generated shim registers the hooks and runs them on every finished `Answer`.

There is no registration ceremony: export the const(s) and the function, and the build wires everything. Under the hood the shim calls `ResponseContract::from_type::<Model>(...)` (plus `.with_public_type::<Public>()` when the two differ) and `.with_answer_hooks(...)` — you never write that code yourself.

## The worked example: hugr-docs

`examples/hugr-docs` answers questions from a read-only docs folder and cites its sources. The design problem: we want users to get *URLs*, but asking the model to construct `https://huggingface.co/docs/...` URLs invites hallucination. The fix is a split contract — the model cites bare paths, and a hook derives the URLs deterministically.

### Two response types

From `examples/hugr-docs/src/lib.rs`:

```rust
pub const RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsResponse";
pub const MODEL_RESPONSE_RUST_TYPE: &str = "hugr_docs::DocsModelResponse";

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

The model sees the schema of `DocsModelResponse` — `related_documents` is just an array of strings, the easiest thing for it to get right. Users, `--config`, and downstream callers see `DocsResponse`, where each document is a `{path, url}` object. `#[serde(deny_unknown_fields)]` on both keeps the cast strict: extra keys from the model are a cast failure, not silent baggage.

### The bridge: answer_hooks()

A hook is a named function that mutates the finished `Answer` in place, after the model output has been cast against the model type:

```rust
pub fn answer_hooks() -> Vec<AnswerHook> {
    vec![AnswerHook::new("hugr_docs::document_urls", |answer| {
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
- **Be defensive about shape.** Hooks work on `serde_json::Value`, so match with `get_mut`/`as_array_mut` and skip quietly rather than unwrap — the hugr-docs hook even handles entries that are already objects, making it idempotent.
- **Give the hook a namespaced name** (`"hugr_docs::document_urls"`): the name identifies the hook in traces and diagnostics.

Hooks must be deterministic pure transformations (string munging, lookups against data you ship — no IO, no clock), because answers are recorded in traces and replay is bit-for-bit. If you need something like the docs URL derivation, this is exactly the tool; if you need a network call, that's a tool for the model, not a hook.

The hook is unit-testable like any function — build an `Answer`, apply `answer_hooks()`, assert on `answer.response` (see the test at the bottom of `examples/hugr-docs/src/lib.rs`).

### Run it

From the repo root:

```bash
export HUGR_DOCS_API_KEY=hf_...
hugr run examples/hugr-docs ./docs "What is the narrow-waist rule?"
```

(`./docs` fills the agent's required positional runtime argument `docs_path`, declared under `[runtime.args.docs_path]` in its `hugr.toml` — it re-jails `fs_read` to that folder per invocation.) The answer's `response.related_documents` comes back as `[{"path": "...", "url": "https://huggingface.co/docs/..."}]` — paths chosen by the model, URLs stamped by the hook.

## Growing your own contract

Take the weather agent from tutorial 1 and evolve it the same way:

1. Widen `Response` in `src/lib.rs` with the fields you want callers to rely on (say `temperature_c: f64`, `conditions: String`). That alone changes the provider schema on the next run — no manifest edit, no rebuild command beyond `hugr run`/`hugr build` as usual.
2. If a field is mechanical (units conversion, formatting, canonical links), move it out of the model's schema: declare a leaner model type, point `MODEL_RESPONSE_RUST_TYPE` at it, and compute the field in a hook.
3. Keep `RESPONSE_RUST_TYPE` naming the *public* type; that is the contract everything downstream — the CLI JSON, MCP, agent-as-tool callers — receives.

The rule of thumb: **the model fills in what only the model knows; hooks fill in what code can derive.** Everything derivable is one less thing the model can get wrong.

## Next

Tutorial 3 is not written yet — meanwhile, good next stops are `examples/hugr-docs/README.md` for runtime args and the MCP serving story, and [agents as tools](../../ARCHITECTURE.md#8-agents-as-tools-composition) for composing the binary you built into a larger agent.
