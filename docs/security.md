# Security

## Security model

**Sandbox-by-registration.** A subagent can invoke only capabilities granted by its manifest. An ungranted tool is never registered, so there is no code path to it. The manifest is the audit surface for human review.

The threat actor is the **model** and any content it reads. Every tool argument is attacker-controlled, and each jail must hold against adversarial arguments.

Tools return semantic errors to the model rather than panicking. A rejected escape attempt therefore becomes another tool result.

The manifest is trusted: a grant's scope is authored by the operator, not the model.

Resource exhaustion beyond documented caps, timing side channels, and anything explicitly granted by the operator are out of scope. Pointing `fs_read` at `/`, for example, is a broad grant and a manifest review failure, not a jail bug.

The operator is responsible for the process and OS boundary when running an untrusted binary.

## Capability threat notes

**`fs_read`** (read-only, one canonicalized root):

- **Path traversal (`../`, absolute, prefix).** Rejected component-wise before any filesystem touch: caller paths must be relative with only `Normal`/`CurDir` components. Test: `jail_rejects_traversal_and_absolute_paths`.
- **Symlink escape.** A symlink inside the root that points outside passes the component check. The defense is the **post-canonicalize `starts_with(root)` re-check** on every resolved target; recursive walks apply the same filter per entry. The root itself is canonicalized at construction. Test: `jail_rejects_symlink_that_escapes_the_root` (unix).
- **TOCTOU on canonicalize.** The window between canonicalization and read is accepted because the tool is read-only. The worst case is reading a swapped file, not writing outside the jail. This is documented but not prevented.

**`fs_write`** (write access, one canonicalized root):

- **Traversal and symlink escape.** Paths reject absolute and parent components. New targets canonicalize and re-check the existing parent; existing removal targets are canonicalized and re-checked before mutation.
- **Destructive scope.** Replacement and removal are intentional powers of the grant. Removal is limited to one file or empty directory, but `root = "/"` still grants writes across the process-visible disk.
- **TOCTOU on canonicalize.** A privileged concurrent process can swap a checked path before mutation. Use an outer OS sandbox when the agent or other local processes are untrusted.

**`shell`** (process execution):

- **Restricted mode.** The capability invokes an exact allowlisted executable with an argument vector. No shell parses `&&`, pipes, redirection, substitutions, expansions, or glob syntax. Arguments can still activate dangerous behavior in an allowed program, so the operator must choose both programs and their OS environment carefully.
- **Full mode.** The capability invokes `<shell> -lc` and provides no Hugr-level sandbox. The model can access every process, file, credential, and network destination allowed to the agent's OS identity.

**`scratchpad`** (per-lineage scratch subtree, ungated, the jail is the boundary):

- **Traversal & symlink escape.** Same discipline as `fs_read`; **writes canonicalize the (created) parent directory too**, so a symlinked parent can't redirect a write outside the jail. Tool results carry only relative paths, so scratch contents never enter the log. Tests: `crates/hugr-agent/tests/scratchpad.rs`.
- **Cross-ask / sibling leakage.** Each ask gets its own working copy, seeded through copy-on-fork from the parent's finalized subtree. A fork sees ancestor notes but never a sibling's writes.
- **Blob hardlinks.** Filesystem-backed `Sha256` inbound blobs may be hardlinked into scratch. Outbound files may be hardlinked into the shared blob store.

  Store objects are read-only. `scratch_write` removes an existing file before replacing it, so overwriting a hardlinked inbound path does not mutate the store object.

  Hashes are capabilities, not secrets. Any agent handed a `sha256:<hash>` can read that object from the shared store.

**`memory`** (agent-wide durable memory, opt-in, persistence is the feature and the risk):

- **Persistence channel.** Content written by one ask can influence unrelated future asks for the same agent. This is useful for notes and equally useful for stored prompt injection, so the grant is opt-in, supports `readonly = true`, and is wipeable by deleting `<agent-home>/memory`.
- **Jail and writes.** Memory uses the same relative-path rejection and post-canonicalization root check as scratch. Filesystem writes are last-write-wins with a process mutex plus an advisory lock file; memory is not a coordination database. Tests: `crates/hugr-agent/tests/memory.rs`.

**`web_fetch`** (network; host allowlist + GET-only default + byte cap; empty allowlist ⇒ fail-closed):

- **Off-allowlist host.** The parsed host must equal an allowlisted host or be a dot-bounded subdomain. Userinfo tricks (`https://allowed@evil.com`) resolve to the real host and are rejected; suffix collisions (`notexample.com` vs `example.com`) are prevented by the `.` boundary.
- **Redirect bypass (SSRF).** Automatic redirects are disabled (`redirect::Policy::none()`); a `3xx` is returned to the model as-is, and following it is a *new* call whose target is re-checked.
- **Scheme confusion.** Only `http`/`https`; `file://` etc. cannot exfiltrate local files.
- **DNS-rebinding / internal-IP SSRF.** Not defended at v1: allowlisting a host that resolves internally reaches it. Mitigation is operator-side; resolve-and-pin is future work.
- **Markdown conversion.** Conversion parses only the returned HTML bytes and executes no scripts. It is a representation change, not a content-safety boundary.

**`web_search`** (network; Exa API):

- **Egress and secrets.** Queries leave the host for Exa. The API key is read from the configured environment variable and sent only to Exa's fixed HTTPS endpoint.
- **Untrusted results.** Titles, snippets, URLs, and requested extracted contents are attacker-controlled web data and must not be treated as instructions.

**`traces_read`** (read-only over an agent home's `traces/` + `feedback/`):

- **Path traversal via trace ids.** Trace ids key file paths (`<id>.json`); ids are validated against a closed character set (ASCII alphanumeric, `-`, `_`) before any filesystem touch, so a crafted id (`../…`, absolute, separators) cannot leave the jail. The root itself is canonicalized at construction. Test: `crafted_trace_id_is_rejected_before_io`.
- **Attacker-influenced content.** Trace transcripts contain model and tool output, while feedback payloads are caller-supplied. Both are untrusted. Any agent granted `traces_read` (e.g. an insights agent) must treat everything it reads as data to analyze, never as instructions to follow; its system prompt should say so explicitly.
- **Cross-agent reading.** The grant's root selects *which* agent's home is readable; granting `~/.hugr/<other-agent>` deliberately exposes that agent's full conversation history to the reader. The manifest line is the audit surface.

**External grants (`mcp`, `agent`).** `[tools.mcp.*]` runs an operator-declared external process. Its jail is the process boundary plus whatever the server enforces. Hugr does not sandbox its filesystem or network.

Granting an MCP server is equivalent to trusting its command. `--config` exposes the command and args for audit.

`[tools.agent.*]` starts a built Hugr agent whose own manifest is its jail. Privileges compose downward only.

`[tools.delegate]` starts the same agent in a fresh subprocess context. It does not attenuate privileges because the child has the same manifest. A cross-process depth budget terminates recursive self-delegation.

**Feedback sidecars.** Feedback payloads are untrusted text/JSON from a caller, often from another model. They are stored append-only outside the trace and are never consumed during an answer, but any later analytics or self-improvement agent that reads `<agent-home>/feedback` must treat the payload as attacker-controlled input.

**Cron jobs.** Recurring asks are host-side automation, not core behavior: the clock lives in the scheduler, each fire is an ordinary `Ask`, and overlap for the same job is skipped. Unattended model calls can spend money without a human watching, so cron serving refuses jobs with no effective `max_cost_micro_usd` unless `--allow-uncapped` is explicit.

**Custom storage backends.** A backend is trusted host code, the same class as a custom capability or model adapter. It sees trace contents, blob bytes, and scratch data for the agent that registers it; Hugr enforces the model-facing jail before calls reach the backend, but it does not sandbox a backend implementation.
