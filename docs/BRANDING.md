# Branding

## Name

**Baton**

A Rust-based agent harness. Branded **Baton**; published under `baton-rs` where the
bare `baton` name is already taken.

### Why "Baton"

"Baton" is an English word borrowed from French *bâton* ("stick"). Two of its senses
map almost perfectly onto what the harness does:

- **The relay-race baton** — the object runners pass from hand to hand. "Passing the
  baton" means handing off work and control cleanly. That is exactly what an agent
  harness does between turns, tools, and sub-agents.
- **The conductor's baton** — the stick used to *coordinate* many players at once.
  A good fit for an orchestrator driving multiple agents.

It is short, easy to remember, pronounceable, and keeps a subtle French flavor.

### Vocabulary

The relay-race metaphor gives a coherent internal vocabulary:

- **baton** — the unit of work/context passed between agents.
- **leg** — one agent's turn (one runner's segment of the race).
- **lap** — a full loop of the agent cycle.
- **handoff** / **exchange zone** — the boundary where context transfers from one
  agent to the next.

The CLI reads naturally: `baton run ...`.

## Naming & namespaces

| Namespace           | Name       | Status                                                  |
| ------------------- | ---------- | ------------------------------------------------------- |
| Brand               | `Baton`    | Chosen.                                                 |
| crates.io (publish) | `baton-rs` | Available. Used because bare `baton` is taken.          |
| crates.io (bare)    | `baton`    | Taken — an unrelated, low-activity async channel crate. |
| GitHub              | `baton-rs` | Available.                                              |
| npm                 | `baton-rs` | Available.                                              |

### Crate naming convention

Derivatives follow `baton-<area>`:

- `baton-core` — runtime.
- `baton-cli` — the `baton` command.
- `baton-hub` — Hugging Face integration.

### Note on discoverability

Search engines will initially surface the unrelated `baton` channel crate and `bat`-family
tooling for queries like "baton rust". This is a known, accepted trade-off for owning a
short, memorable name over time.
