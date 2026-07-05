# Threat model — the predefined tool library (ROADMAP T3.6)

Companion to `ARCHITECTURE.md` §20.2. Hugr's security model is **sandbox-by-registration**: a subagent can only invoke a capability that its manifest grants (ARCHITECTURE §7.1), and each library tool is jailed to its declared scope. This document records, per tool, what the jail defends against, the escape vectors reviewed, and the regression tests that pin them. It is scoped to the *tool* boundary; the process/OS boundary (running an untrusted binary) is the operator's responsibility, and `code_exec` (the only exec-class tool, ROADMAP T5.6) is out of scope until it lands.

## Assumptions & non-goals

- The **manifest is trusted** — it is the audit surface a human reviews. A grant's scope (an `fs_read` root, an `http_fetch` allowlist, a `sqlite_query` file) is authored by the operator, not the model. The threat actor is the **model** (and any content it reads): the jail must hold even when the model emits adversarial tool arguments.
- The **model output is untrusted.** Every tool argument is attacker-controlled. Tools return *semantic* errors to the model (never panics), so a rejected escape attempt is just another tool result the model sees.
- **Not defended here:** resource exhaustion beyond the documented caps, timing side channels, and anything the operator explicitly grants (allowlisting a TLD, pointing `fs_read` at `/`). Granting broadly is a manifest review failure, not a jail bug.

## `fs_read` (read-only filesystem) — privilege class `read_only`

Scope: one canonicalized root directory. Registers `fs_list` / `fs_search` / `fs_read` / `fs_read_range` / `fs_read_many` / `fs_outline`, all sharing the same `FsRoot` jail.

- **Path traversal (`../`, absolute, root, prefix).** Rejected component-wise before any filesystem touch: a caller path must be relative and contain only `Normal`/`CurDir` components. Test: `jail_rejects_traversal_and_absolute_paths`.
- **Symlink escape.** A symlink *inside* the root pointing *outside* has only `Normal` path components, so it clears the component check — the defense is the **post-canonicalize `starts_with(root)` re-check**: `resolve_existing` canonicalizes the candidate (resolving symlinks) and rejects any target that no longer lives under the canonical root. The root itself is canonicalized at construction, so a symlinked root can't widen the jail. Recursive walks (`fs_list`, `fs_outline`) apply the same `canonicalize().starts_with(root)` filter to every entry. Test: `jail_rejects_symlink_that_escapes_the_root` (unix).
- **TOCTOU on canonicalize.** The window between canonicalization and read is not closed by the tool (that would require `openat`-family syscalls); it is accepted because the tool is **read-only** — the worst case is reading a file that was swapped under a path already proven in-jail, not writing outside it. Documented, not defended.

## `scratchpad` (per-lineage scratch) — privilege class `scratchpad`

Scope: the ask's working scratch subtree (ARCHITECTURE §19.3). Registers `scratch_read` / `scratch_write` / `scratch_list`, provided by the agent runtime (`hugr-agent`), not this library. Ungated (the jail is the boundary).

- **Path traversal & symlink escape.** Same discipline as `fs_read`: absolute/`..`/root/prefix components rejected, canonical target re-checked against the scratch root. **Writes canonicalize the (created) parent directory too**, so a symlinked parent can't redirect a write outside the jail. Tool results carry only *relative* paths, so scratch contents never enter the log (replay stays deterministic and traces stay portable). Tests: `crates/hugr-agent/tests/scratchpad.rs`.
- **Cross-ask / sibling leakage.** Each ask gets its own `.pending/<pid>-<n>` working copy, seeded copy-on-fork from the parent's finalized subtree — so a fork sees ancestor notes but never a sibling's writes.

## `http_fetch` (network egress) — privilege class `network`

Scope: a host allowlist + method allowlist (GET-only default) + response byte cap. Empty allowlist ⇒ fail-closed (every request denied).

- **Off-allowlist host.** The URL's parsed host must equal an allowlisted host or be a dot-bounded subdomain of one. Userinfo tricks (`https://allowed@evil.com`) resolve to the real host (`evil.com`) and are rejected. Suffix-collision (`notexample.com` vs `example.com`) is prevented by the required `.` boundary. Tests: `host_allowlist_matches_exact_and_subdomains`, `a_bare_host_does_not_match_a_different_host_with_shared_suffix`, `userinfo_and_nonhttp_schemes_cannot_bypass_the_allowlist`.
- **Redirect bypass (SSRF).** The reviewed gap: `reqwest` follows up to 10 redirects by default, and the allowlist is only checked on the *initial* URL — so an allowlisted host could `3xx`-redirect to an off-allowlist or internal target. **Fixed:** automatic redirects are disabled (`redirect::Policy::none()`); a `3xx` is returned to the model as-is, and following it is a *new* `http_fetch` call whose target is re-checked. Documented in the module header.
- **Scheme confusion.** Only `http`/`https` are accepted, so `file://`/`ftp://` etc. cannot exfiltrate local files. Test: `userinfo_and_nonhttp_schemes_cannot_bypass_the_allowlist`.
- **DNS-rebinding / internal-IP SSRF.** Not defended at v1: if an operator allowlists a host that resolves to an internal address, requests reach it. Mitigation is operator-side (don't allowlist internal hosts); a resolve-and-pin or IP-denylist layer is future work.

## `sqlite_query` (read-only SQLite) — privilege class `read_only`

Scope: one canonicalized database file, opened `SQLITE_OPEN_READ_ONLY`, per-call connection on a blocking thread, row cap. (Behind the `sqlite` cargo feature.)

- **Second-file access via `ATTACH`.** Even a read-only main connection can `ATTACH DATABASE` another readable file, escaping the one-file scope. Rejected before the query runs by a **token-based** check (`attach` as a whole SQL word, any casing/spacing) — an identifier like `attachment` is not a false positive. Tests: `attach_keyword_is_detected_across_casing_and_spacing`, `substring_attach_in_an_identifier_is_not_a_false_positive`.
- **Writes / DDL.** Fail at the engine because the connection is read-only, regardless of SQL text.
- **Symlinked file path.** The manifest `file` is canonicalized at construction and the tool is bound to that one resolved path for its lifetime.
- **Reviewed residual.** The `ATTACH` defense is a SQL-text guard, not an engine authorizer; an engine-level authorizer callback (deny `SQLITE_ATTACH`) is a possible future hardening but was not added at v1 to avoid depending on a specific `rusqlite` authorizer API. The read-only open + single-file scope remain the primary controls.

## External-tool grants (`mcp`, `plugin`)

`[tools.mcp.*]` and `[tools.plugin.*]` run an operator-declared external process (ARCHITECTURE §20.3). Their jail is the process boundary and whatever the server/plugin itself enforces — Hugr does not sandbox their filesystem/network access. Granting one is equivalent to trusting that command; this is a manifest-review decision, and such grants are surfaced by `--config` (T3.5) with their command/args for audit.
