# Built-in capabilities

This page lists the capabilities provided by `huggr-toolkit`. Grant-driven capabilities are registered only when their manifest grant is present; scratchpad capabilities are part of every ask. Relative filesystem roots resolve from the agent crate, and `root = "/"` explicitly grants the full filesystem while tool paths remain relative to that root.

## Filesystem reads

`[tools.fs_read]` accepts `root` (default `.`) and registers eight read-only capabilities under that canonicalized root.

| Capability | What it does | Limits |
| --- | --- | --- |
| `fs_list` | Lists a directory, optionally recursively. | At most 2,000 returned entries; recursive walks have an internal 20,000-file ceiling. |
| `fs_search` | Finds a case-insensitive literal substring with paths, line numbers, and snippets. | Text-like files up to 512 KB; at most 500 matches per call. |
| `fs_grep` | Matches a Rust regular expression, optionally case-insensitively. | Same file and match limits as `fs_search`. |
| `fs_glob` | Matches relative file paths with a glob; `**` crosses directories. | At most 2,000 returned paths; internal 20,000-file walk ceiling. |
| `fs_read` | Reads one text file. | Default 200 KB, maximum 1 MB. |
| `fs_read_range` | Reads an inclusive 1-based line range. | Default 200 lines, maximum 5,000 lines and 1 MB. |
| `fs_read_many` | Reads several text files. | At most 50 files and 1 MB per file. |
| `fs_outline` | Extracts Markdown-style headings from a file or directory. | Configurable document and heading caps. |

Absolute tool paths, `..`, and symlink escapes are rejected. A full-disk grant uses `[tools.fs_read] root = "/"`; for example, pass `etc/hosts` to read `/etc/hosts`.

## Filesystem writes

`[tools.fs_write]` accepts `root` (default `.`) and registers `fs_write`, `fs_create_dir`, and `fs_remove`. `fs_write` creates, replaces, or appends to one file whose parent already exists. `fs_create_dir` creates one directory whose parent exists. `fs_remove` removes one file or one empty directory and never removes recursively.

Write targets and their canonicalized parents must remain under the configured root, including through symlinks. Use `root = "/"` only when the operator intends to grant full-disk writes.

## Shell

Restricted mode invokes an allowlisted executable directly with an argument array. It does not invoke a shell, so `&&`, pipes, redirection, command substitution, environment expansion, and glob expansion are not interpreted.

```toml
[tools.shell]
allow_commands = ["git", "cargo"]
cwd = "."
max_output_bytes = 1000000
```

The `command` must exactly match one entry in `allow_commands` and cannot contain whitespace. Full mode passes `command` to `<shell> -lc`:

```toml
[tools.shell]
full_access = true
shell = "/bin/sh"
cwd = "."
max_output_bytes = 1000000
```

Full mode is arbitrary process and filesystem access under the agent's operating-system identity. Huggr does not sandbox it; use an outer container, VM, OS sandbox, or a trusted agent when isolation is required. `cwd` is optional in either mode. Stdout and stderr are returned separately and capped independently.

## Web fetch

`[tools.web_fetch]` registers `web_fetch`. `allow_hosts` matches exact hosts plus subdomains. `allow_methods` defaults to `GET`; `max_bytes` defaults to 1 MB. An empty host list denies every request, automatic redirects are disabled, and only HTTP(S) URLs are accepted.

Set `markdown = true` in the manifest to convert every returned HTML body to Markdown, or pass `markdown` per call to override the default. The result reports `format = "markdown"` or `"raw"`. Conversion may omit browser-rendered content because `web_fetch` does not execute JavaScript.

## Web search

`[tools.web_search]` registers `web_search` backed by Exa's search API. `api_key_env` defaults to `EXA_API_KEY`; the secret is read from that environment variable. `max_results` defaults to 10 and is capped at 100. Calls accept a query, an optional result count, and `contents = true` to request extracted page text. Grant `web_fetch` separately when the agent must fetch result URLs.

## Delegation

`[tools.delegate]` registers `delegate`, which starts the current built agent as a subprocess with a fresh context and the same manifest. Its arguments use the standard `Ask` shape: `question`, optional `trace_id`, and optional blob handles. The child writes its own immutable trace, its metadata folds into the parent's cost, and the existing depth budget stops recursive self-calls after three nested delegations by default.

The default artifact is the current executable, which suits CLI agent binaries and generated development shims. Set `artifact` to an explicit CLI agent binary when the current process is the generic legacy `huggr run` host or a language host such as Python. Self-delegation uses the same privileges; use `[tools.agent.<name>]` for a child with different grants.

## State and inspection

Every ask registers per-lineage `scratch_read`, `scratch_write`, and `scratch_list`; `[tools.scratchpad]` is an optional audit marker. `[tools.memory]` registers agent-wide `memory_read`, `memory_write`, and `memory_list`; with `readonly = true`, writes return a semantic error. `[tools.traces_read]` registers `trace_list`, `trace_ops`, `trace_transcript`, and `feedback_list` under one agent home with size and paging caps. Treat persisted content as untrusted data.

## External capabilities

`[tools.agent.<name>] artifact = "..."` registers `agent_<name>` and `agent_<name>_feedback` for a different built Huggr agent. `[tools.mcp.<name>]` starts an operator-declared stdio MCP server and registers its discovered, namespaced tools. Both are subprocess boundaries. An MCP server and a full shell are trusted operator grants and can perform anything allowed by their operating-system environment.
