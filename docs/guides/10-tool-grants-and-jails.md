# Tool grants and jails

This guide explains how a huglet gets its tools and why it cannot use anything else: how sandbox-by-registration works, what each grant in the tool library does, how to scope it, and where each jail's boundary actually is. The full option tables live in [built-in capabilities](../capabilities.md) and the per-capability threat notes in [security](../security.md); this guide shows how to use them together when authoring a manifest.

## The problem

An agent's power is exactly its tool set. Give a docs-answering agent a shell and it can exfiltrate credentials the moment a prompt injection lands; give it nothing but a jailed read of one folder and the worst a hostile input can do is read that folder. The question every agent author has to answer is: what is the smallest set of tools that still does the job, and what is the blast radius if the model misuses every one of them?

Huggr makes that question answerable by review of one file. The manifest is the complete list of what the agent can do.

## Sandbox-by-registration

The core never executes anything. When the model calls a tool, the brain emits `StartCapability { name, args }` and the host looks the name up in its `CapabilityRegistry`. The toolkit only registers capabilities whose grant appears in `huggr.toml`, so a tool that is not granted has no code path: there is nothing to invoke, no policy to bypass, no flag to flip at runtime.

Two consequences are worth internalizing:

- **The manifest is the audit surface.** "This agent has no shell" is a fact about registration you can verify by reading `[tools]`, not a hope that a runtime check holds. `--describe` and `--config` on a built binary show the same facts for a shipped artifact.
- **The model is the threat actor.** Every tool argument is attacker-controlled, because model output can be steered by anything the model reads. Each capability therefore validates its arguments against its declared scope and returns a semantic error to the model on a violation; a rejected escape attempt is just another tool result.

One grant can register several capabilities. `[tools.fs_read]` registers the eight `fs_*` read tools; `[tools.agent.receipts]` registers `agent_receipts` and `agent_receipts_feedback`. What the model sees is the union of the capabilities behind the grants.

## Filesystem grants

`fs_read` and `fs_write` are each jailed to one canonicalized root, declared per grant:

```toml
[tools.fs_read]
root = "./policies"        # relative roots resolve from the agent crate

[tools.fs_write]
root = "./output"
```

`fs_read` registers the read-only family: `fs_list`, `fs_search`, `fs_grep`, `fs_glob`, `fs_read`, `fs_read_range`, `fs_read_many`, and `fs_outline`, each with fixed size and match caps (200 KB default reads, 1 MB hard cap, 2,000 entries per listing). `fs_write` registers `fs_write`, `fs_create_dir`, and `fs_remove`; removal takes one file or one empty directory and is never recursive.

The jail works the same way in both: tool paths must be relative, `..` and absolute paths are rejected before any filesystem touch, and every resolved target is canonicalized and re-checked against the root, so a symlink inside the root that points outside does not escape. `root = "/"` is an explicit full-disk grant, not a misconfiguration the jail softens; if you write it, you mean it.

Scope roots per purpose rather than per agent. An agent that reads policies and writes reports gets a read grant on the policies folder and a write grant on the reports folder, not one wide grant covering both.

## Shell: restricted and full

`shell` is the grant to think hardest about, and it has two deliberately different modes.

Restricted mode executes an exact allowlisted program with an argument vector and no shell in between:

```toml
[tools.shell]
allow_commands = ["git", "cargo"]
cwd = "."
max_output_bytes = 1000000
```

Because no shell parses the command, `&&`, pipes, redirection, substitution, and glob expansion are not interpreted; the model gets `git` with arguments, nothing more. The remaining risk is the allowlisted programs themselves: arguments can still make an allowed program do damage (`git push`, `cargo run`), so choose programs whose worst case you accept.

Full mode passes the command to `<shell> -lc`:

```toml
[tools.shell]
full_access = true
```

This is arbitrary process, file, and network access under the agent's OS identity, and Huggr does not pretend otherwise: there is no Huggr-level sandbox in full mode. Use it only inside an outer boundary you trust (container, VM, OS sandbox), and prefer restricted mode or a narrower grant whenever the task allows.

## Web grants

`web_fetch` is allowlist-first and fails closed:

```toml
[tools.web_fetch]
allow_hosts = ["api.open-meteo.com"]   # exact hosts plus dot-bounded subdomains
markdown = true                        # convert HTML responses to Markdown
```

An empty allowlist denies everything, only `http(s)` URLs are accepted, methods default to GET, and responses are capped at 1 MB by default. Redirects are not followed automatically: a `3xx` goes back to the model, and following it is a new call whose target is re-checked against the allowlist, which closes the classic redirect SSRF. Host matching resolves the real host, so `https://allowed@evil.com` does not pass.

`web_search` registers an Exa-backed search; the API key is read from the environment variable named by `api_key_env` (default `EXA_API_KEY`) and sent only to Exa. Search results are untrusted web content. Grant `web_fetch` separately if the agent should also fetch the result URLs.

## State and introspection grants

- `scratchpad` is always available and needs no grant; `scratch_read`/`scratch_write`/`scratch_list` are jailed to the ask's own scratch subtree.
- `[tools.memory]` opts into durable agent-wide notes; `readonly = true` registers only the read side. Persistence is the feature and the risk: memory written under one ask influences future asks, which is exactly what stored prompt injection wants, so grant writes only when the agent's job needs them.
- `[tools.traces_read]` exposes one agent home's stored traces and feedback as paged, size-capped summaries; granting it against another agent's home deliberately makes that agent's full history readable. Everything it returns is untrusted data to analyze, never instructions to follow.

[Guide 12](12-blobs-scratchpad-memory.md) covers these three in depth.

## External-process grants

Three grants cross the process boundary, and their jail is the manifest of what is on the other side, not a Huggr filesystem check:

- `[tools.agent.<name>]` grants another built huglet. The child runs under its own manifest, jail, and limits; privileges compose downward only. See [composition and cost](07-composition-and-cost.md).
- `[tools.mcp.<name>]` starts an operator-declared stdio MCP server. Granting it is equivalent to trusting its command; Huggr does not sandbox what the server does.
- `[tools.delegate]` restarts the same agent in a fresh subprocess context with the same privileges, depth-capped.

Treat these lines in a manifest review the way you would treat `full_access = true`: the question is not "is the jail tight" but "do I trust that program".

## Worked example

A changelog agent that reads a repository, runs read-only git commands, and writes one summary file:

```toml
[agent]
name = "changelog"
version = "0.1.0"
description = "Summarizes recent changes in a repository."

[models]
base_url = "https://router.huggingface.co/v1"
api_key_env = "HUGGR_API_KEY"
[models.default]
model = "google/gemma-4-31B-it:cerebras"
input_usd_per_m_tokens = 1.0
output_usd_per_m_tokens = 1.5

[tools.fs_read]
root = "."

[tools.shell]
allow_commands = ["git"]

[tools.fs_write]
root = "./reports"
```

The review reads in one pass: this agent can read the repo, run `git` (including, note, `git push` if the environment has credentials), and write inside `reports/`. It cannot fetch URLs, cannot run any other program, and cannot write outside `reports/`. If `git`'s write subcommands are unacceptable, the fix is environmental (a read-only checkout or credential-free environment), because restricted mode allowlists programs, not subcommands.

## Limitations

- Jails constrain the model, not trusted code. Python or TypeScript tool callables, custom Rust capabilities, and storage backends are trusted host code and run unjailed.
- Read-side canonicalization accepts a TOCTOU window: a file swapped between check and read is read. For `fs_write` the same window exists against privileged concurrent processes; use an OS sandbox when other local processes are untrusted.
- `web_fetch` does not defend DNS rebinding: an allowlisted host that resolves to an internal address is reachable. Allowlist only hosts you trust to stay external.
- Restricted shell cannot express "this program but only these subcommands"; that granularity belongs to the program or the environment.
- Resource exhaustion below the documented caps and anything the operator explicitly granted are out of scope by design; the manifest is the last line of review.
