# Configure runtime arguments

This guide explains `[runtime.args.<name>]`, the mechanism for invocation-time huglet arguments: what it can patch, how values flow from the CLI, environment, or MCP call into the manifest before the agent is assembled, and how each surface exposes the declared arguments. It is what lets a single built binary serve different data or scopes per invocation without recompiling. Model catalogs have their own host-level override mechanism.

## The problem

A manifest is compile-time data: `huggr build` embeds it into the binary. But some values are only known at the call site. The canonical case is the docs agent, one binary that should answer questions about *whatever folder the caller points it at*, with `fs_read` jailed to that folder and nothing else. Hardcoding the path means one build per folder; accepting a free-form path as a tool argument would put the jail boundary in the model's hands.

Runtime args resolve this by patching the *manifest* before assembly. The caller supplies a value, the declared target field is rewritten, and only then are tools registered and jails constructed. The model never sees the mechanism; it just finds itself in a differently-scoped sandbox.

## Declaring an argument

```toml
[runtime.args.docs_path]
target = "tools.fs_read.root"     # the manifest field this value patches
positional = true                 # expose before the question instead of as --docs-path
required = true
env = "HUGGR_DOCS_PATH"           # environment fallback
help = "Folder containing documentation to search."
```

The keys are `target` (required), `help`, `positional`, `required`, `env`, `default`, and `flag` (overrides the derived flag name). Values are strings; the two pricing targets are parsed to numbers, everything else is patched in verbatim. Precedence per invocation is explicit value, then the `env` variable, then `default`.

Targets are a deliberately closed set, not arbitrary dotted paths:

- `tools.<grant>.<key>`, including `tools.mcp.<name>.<key>` and `tools.agent.<name>.<key>`
- `models.<tier>.model`, `models.<tier>.input_usd_per_m_tokens`, and `models.<tier>.output_usd_per_m_tokens` for a tier already resolved into the definition
- `traces.store` and `scratchpad.root`

Anything else fails validation with the list of supported targets. The set covers what legitimately varies per invocation (scopes, concrete model fields, state roots) while keeping the rest of the manifest, notably which tools are granted at all, fixed at build time. Prefer the global model catalog or an explicit runtime catalog over declaring model runtime arguments.

Path-like values (tool roots, artifacts, `traces.store`, `scratchpad.root`) are resolved from the **caller's** working directory when relative, so `./docs` means the caller's `./docs`, which is what a CLI user expects and what makes the same binary usable from anywhere.

## What each surface generates

One declaration fans out to every surface the toolkit generates:

- **CLI.** A positional argument placed before the question (in declaration order, sorted by name), or a `--docs-path` style flag derived from the name with underscores turned into hyphens. `help` becomes the help text. `huggr run <agent-dir>` accepts the same arguments during development, so the dev loop and the shipped binary parse identically.
- **MCP.** Each argument becomes a string property on the `ask` tool's schema, listed as required when declared required, so an MCP client can re-scope each call (see [serving and consuming MCP](mcp.md)).
- **Generated Python wrapper.** Positional args become leading positional `str` parameters of `ask(...)`, before `question`; optional ones become keyword `Optional[str]` parameters; `help` lands in the docstring. A type checker enforces them like any other typed API.

A missing required argument on the ask path produces the standard `status: "error"` answer at exit 0. Introspection surfaces (`--describe`, `--config`, `--traces`, `--stats`) and server startup (`--mcp-serve`) treat every argument as optional, because describing or starting the agent should not require ask-time values; MCP enforces required arguments per `ask` call instead.

Names must not collide with the built-in surface flags (`question`, `trace`, `json`, `blob`, and the rest), and validation rejects unknown keys in the declaration, so a typo fails the build rather than silently doing nothing.

## Patching happens before assembly

The order of operations is the security property. Values are validated and written into the (in-memory) manifest first; then tools are registered from the patched manifest and jails canonicalize their roots. By the time a model turn starts, `docs_path` has become an ordinary `fs_read` jail like any hardcoded one, with the same traversal and symlink defenses ([tool grants and jails](../concepts/tool-security.md)).

Whoever supplies runtime arguments is doing operator-level configuration. A caller who can pass `docs_path` can point the read jail anywhere their filesystem allows, and a caller who can patch a model id can select what the configured provider serves. That is the intended contract, arguments are the operator's knobs surfaced to the caller, but it means you should only declare targets you are willing to hand to every caller of the binary, including MCP clients.

## Worked example

The reference docs huglet declares exactly the `docs_path` argument above. One build yields:

```bash
huglet-docs ./docs "How do I grant a tool?"          # CLI, positional
HUGGR_DOCS_PATH=./docs huglet-docs "How do I …?"     # environment fallback
huglet-docs ./docs --mcp-serve                        # MCP server jailed to ./docs
```

and, built with `--surface python`:

```python
import huglet_docs
answer = huglet_docs.ask("./docs", "How do I grant a tool?")
# The async huglet_docs.run("./docs", ...) event stream uses the same arguments.
```

Four call sites, one artifact, and in every case `fs_read` was re-jailed before the first model call. Point it at a different folder tomorrow and the traces, scratch, and memory still accumulate under the same agent home, because identity comes from the agent name, not the argument.

## Limitations

- Values are flat strings. There are no typed or repeated arguments, and no boolean flags; encode structure in what the target field accepts.
- Targets can only repoint existing grants and settings. A runtime argument cannot add a tool, remove one, or change `[limits]`; the grant list is fixed at build time by design.
- Required-ness is enforced on the ask path only; a server can start without values, so a misconfigured registration surfaces on the first call rather than at startup.
- Precedence is fixed (explicit, then `env`, then `default`); there is no per-surface override order.
