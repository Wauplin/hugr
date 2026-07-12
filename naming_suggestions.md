# Naming suggestions

Replacement names for `huggr`, which is taken on crates.io and PyPI. The names here are meant to read naturally to English speakers, stay short and quick to type, and keep the owner's stated preference: **ants for the individual agents**. Where possible they also nod to the optional associations the owner called out (rust/oxide imagery, a light many-small-agents or brain feel). A French flavor was considered and set aside: it made the leading names hard to read for English speakers.

Every name marked free below returned no project from the crates.io sparse index and the PyPI JSON endpoint on 12 July 2026, and a web search found no prominent software, AI, or CLI product using the exact spelling. This is a point-in-time registry check, not a reservation, trademark search, or domain clearance. Words used only as product vocabulary (ant, trail, colony) are not availability claims.

The English field is crowded: most single common words (`swarm`, `colony`, `hive`, `flock`, `forge`, `ember`, `patina`) are already taken on one or both registries, so the shortlist is deliberately small and each entry is checked.

## Recommendation

Lead with **`throng`** for the framework and **ant** for the individual agent. `throng` is a plain English word for a large crowd of many, which matches the project's core idea, and it is free on both registries with no software collision found.

| Role | Term | Notes |
| --- | --- | --- |
| Product, CLI, root package | **`throng`** | English, "a crowd of many." Free on crates.io and PyPI. Six letters, one syllable, easy to type and say. |
| One self-contained agent | an **ant** | Keeps the owner's ant preference and stays literal in API prose. |
| A capability or plugin | a **trail** | Ants coordinate by laying trails; the API can still say "tool" or "capability" where a metaphor would obscure it. |
| A running group of agents | a **colony** | The natural collective for a multi-agent composition. |
| Package family | `throng-core`, `throng-host`, `throng-agent`, `throng-python` | |

```console
throng new weather
throng run weather
throng build weather
throng traces weather
```

Prose stays clear: "create an ant from a manifest," "grant the ant filesystem and web trails," "run a colony of ants with throng." The only mild cost is that `throng` names a crowd generally rather than an ant colony specifically, so the ant vocabulary does the theming.

## Alternatives, by what they optimize for

### Most literal and on-theme: `anthill`

`anthill` is instantly understandable and pairs perfectly with ants as agents: the framework is the anthill, each agent is an ant, a running set is a colony. It is free on crates.io but taken on PyPI, so it works cleanly only if the project is Rust-first and accepts a different PyPI distribution name such as `anthill-agents`.

- Product and CLI: `anthill` (`anthill run weather`)
- Individual agent: an **ant**; plugin: a **trail**; group: a **colony**
- Caveat: seven letters, and the PyPI name split.

### Army of small specialists: `cohort`

`cohort` is an English word for an organized band, originally a unit of a Roman legion, which fits "an army of small specialists working together." Free on crates.io, taken on PyPI (same `cohort-agents` workaround). Short and easy to type.

- Product and CLI: `cohort`
- Individual agent: an **ant** or, if you drop the ant metaphor, a **member**
- Caveat: "cohort" is also a common analytics term (cohort analysis), so it carries a mild second meaning.

### Best oxide imagery: `umber` or `russet`

Both are ordinary English colour words tied to iron oxide, so they are genuine rust references rather than loose ones, and both are free on crates.io and PyPI.

- `umber`: a brown earth pigment coloured by iron oxide. Excellent five-letter command: `umber run weather`. Minor unrelated uses exist (an old Java library, a consultancy) but nothing prominent in this space.
- `russet`: a reddish-brown, also an apple and potato variety. Six letters, clearly English.
- Caveat for both: a colour name supplies mood and the oxide tie-in but no built-in collective/individual story, so pair it with plain nouns (agent, tool, group), and the ants can still be the agents.

## Set aside

Registry-free or nearly so, but rejected for the reason given:

- `teem` (as in "teeming with ants") reads well and is short, but the space is crowded: Teeme.ai is an AI-agent platform, and several other products use "Teem."
- `swarm`, `colony`, `hive`, `flock`, `forge`, `ember`, `patina`, `ochre`: strong English words, all taken on one or both registries.
- French words such as `essaim` (swarm), `calin` (hug), `galet` (pebble), and `fourmi` (ant): pleasant and mostly free, but they read awkwardly for English speakers, which the owner ruled out. `calin` remains the closest French successor to `huggr` and the strongest keeper of the Hugging Face "hug" association if that constraint ever outweighs English readability.
- `eciton` (the army-ant genus, the most on-target meaning found) is already used by a drone/edge-computing company, an FPGA inference accelerator, and a logistics-AI startup.

## Method and limitations

Availability was checked against the crates.io sparse index path per crate and the PyPI `GET /pypi/<project>/json` endpoint per project; a name is called free only when both returned no project. PyPI normalizes case and runs of `-`, `_`, `.`, which does not affect the plain lowercase names here. Registry availability changes at any time and publishing is what secures a name, so an actual publish attempt is the final test. None of this constitutes trademark, organization-name, social-handle, or domain clearance; complete those before committing to a final name.
