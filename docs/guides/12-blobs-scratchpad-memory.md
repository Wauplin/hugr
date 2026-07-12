# Files and state: blobs, scratchpad, and memory

This guide explains how a huglet works with files and persistent state: how a caller hands files in and gets files back (blobs), where the agent keeps working state that follows a conversation (the scratchpad), and how it keeps notes that outlive any conversation (memory). The three mechanisms have different lifetimes and different trust properties, and picking the right one is most of the design work.

## The problem

An agent that only exchanges JSON text with its caller hits three walls. Large payloads do not belong in a model context or a trace; an agent that computes something across several tool calls needs somewhere to put intermediate results, especially when the caller may resume the conversation later; and some agents genuinely benefit from remembering facts across unrelated conversations. Stuffing any of these into chat history makes traces balloon and context grow, and gives the data the wrong lifetime.

Huggr separates them by lifetime:

| Mechanism | Lifetime | Scope |
| --- | --- | --- |
| Blobs | permanent, content-addressed | shared across agents, explicit hand-off |
| Scratchpad | one trace lineage | private to the ask and its resumes/forks |
| Memory | the agent, forever | shared across all of the agent's asks |

## Blobs: files across the contract boundary

A `BlobHandle` is a file reference plus a `media_type` and an optional `name` hint. Its `ref` takes one of three shapes: inline `bytes` (base64 on the wire), a `path` readable by the receiving host, or `sha256:<hex>`, a content address into the shared blob store.

**Inbound.** `Ask.blobs` files are materialized into the agent's scratchpad before the turn starts, under the handle's `name` hint (sanitized to a single path segment) or a derived stable name. The runtime guidance appended to the system prompt tells the model that attached files live in the scratchpad, so it finds them with `scratch_list` and `scratch_read`. On the CLI the flag is repeatable and overloaded:

```bash
my-agent "Summarize the attachment" --blob ./report.pdf          # local file, media type guessed
my-agent "Compare with the baseline" --blob sha256:ba7816bf...   # existing stored object
```

**Outbound.** The convention is one directory name: the agent writes caller-facing files under `out/` in its scratchpad. After the turn, Huggr sweeps `out/` recursively, stores each file in the content-addressed blob store, and returns one handle per file on `Answer.blobs`, named by its path relative to `out/`. Files the agent writes anywhere else in the scratchpad stay private working state.

**The store.** The default store is shared at `~/.huggr/blobs` (override with `HUGGR_BLOB_STORE`, or `HUGGR_HOME/blobs`). Objects are keyed by SHA-256 and installed atomically as read-only files, so identical content deduplicates to one object no matter how many agents produce it. When the store and scratchpad share a filesystem, an inbound `sha256` blob is hardlinked into the scratch rather than copied, which is what makes passing a large file between agents effectively free: a parent granting `[tools.agent.<name>]` forwards `sha256` refs as `--blob sha256:<hash>` and the bytes never cross the process boundary (see [composition and cost](07-composition-and-cost.md)).

Two properties matter for design. The log and trace only ever hold the small reference, never the bytes, so traces stay message-sized. And a hash is a capability, not a secret: any agent handed a `sha256:` ref (or able to guess one) can read that object from the shared store.

## Scratchpad: state that follows the lineage

Every ask gets a scratchpad, no grant needed: `scratch_write` (path + content, creates parent directories), `scratch_read` (UTF-8 text), and `scratch_list`. All three are jailed to the ask's own subtree with the same traversal and symlink discipline as `fs_read`, and none requires a permission round trip.

What makes the scratchpad more than a temp dir is its lifetime, which is tied to the trace lineage:

- A fresh ask starts with an empty working directory; when the ask finishes, the directory is finalized under the new trace id (default root `<agent-home>/scratch`).
- A resumed ask (`trace_id` set) starts from a **copy** of the parent trace's finalized scratch, so notes written in one ask are readable in the follow-up.
- A fork (two asks resuming the same parent) gives each branch its own copy. Siblings never observe each other's writes; this is copy-on-fork, made once when the ask starts.

The runtime guidance tells the model exactly this: keep reusable working state in the scratchpad rather than in chat history, because a caller may resume by trace id. It combines well with [context compaction](09-context-compaction.md): a forgotten or summarized-away tool result can be re-derived from a scratch note, and the note costs no context until read.

Scratch contents never enter the log; tool results carry only relative paths. What persists on disk per trace is the finalized subtree, which is also how `depends_on` lineage stays meaningful for state, not just conversation.

## Memory: state that outlives conversations

`[tools.memory]` is the opt-in third tier: `memory_read`, `memory_write`, and `memory_list`, jailed to `<agent-home>/memory` by default. Unlike scratch, memory is agent-wide: anything written under any ask is visible to every future ask, in any lineage.

```toml
[tools.memory]
readonly = false     # readonly = true registers only reads; writes become semantic errors
```

That persistence is both the feature and the risk. A support agent that records "the staging URL changed" once stops re-asking; equally, a prompt-injected write under one ask can steer unrelated future asks. The runtime guidance therefore instructs the model to write only stable, reusable facts and to treat stored content as untrusted data, not instructions. Grant `readonly = true` when the operator curates memory out of band, and wipe it by deleting the `memory/` directory. Writes are last-write-wins behind a process mutex plus an advisory lock file; memory is a notes folder, not a coordination database.

## Choosing between them

- Result the caller should receive → write it under `out/`; it returns as an outbound blob.
- Intermediate work, or anything a follow-up question should see → scratchpad.
- A stable fact useful to unrelated future asks → memory, and only with the grant.
- A large payload passed between agents → blob refs, so only hashes travel.

## Worked example

A report agent is asked to analyze a CSV. The caller runs `report-agent "Find the top regressions" --blob ./bench.csv`. Before the turn, `bench.csv` is materialized into the scratchpad. The model lists the scratchpad, reads the file, writes `notes/outliers.md` as working state and `out/regressions.md` as the deliverable. After the turn, the answer carries one blob handle named `regressions.md` with a `sha256` ref; the caller fetches it or forwards the ref to another agent unchanged.

The user then asks a follow-up with the returned `trace_id`. The new ask's scratchpad starts as a copy of the previous one, so `notes/outliers.md` and `bench.csv` are still there; nothing is re-uploaded and nothing was hauled through the model context to survive the resume.

## Limitations

- `scratch_read`, `memory_read`, and skill files handle UTF-8 text; binary blobs can be materialized into the scratchpad and returned from `out/`, but the model cannot read their bytes through the scratch tools. Give binary formats to a capability that can parse them.
- Outbound media types are guessed from file extensions, with `application/octet-stream` as the fallback.
- `Bytes` refs do not forward across the agent-as-tool subprocess boundary; hand children `path` or `sha256` refs.
- When the model fills in `blobs` for `agent_<name>`, `delegate`, or an MCP `ask`, `path` refs are accepted only for files inside the calling agent's `fs_read` roots and `sha256` refs must be well-formed content addresses. Orchestrator hand-ins (`--blob` on the CLI, `Ask` from host code) are trusted and unrestricted.
- The blob store has no garbage collection or access control beyond the filesystem; it grows until the operator prunes it, and every local agent sharing the store can read any object by hash.
- Copy-on-fork copies the whole finalized subtree at ask start; a scratchpad holding gigabytes makes every resume pay that copy.
