# Naming suggestions for Hugr

This page collects possible replacements for the `hugr` project name. It favors short, memorable names connected to small specialized agents, collective intelligence, brains, mythology, or playful common-language imagery.

Every suggested name on this page returned HTTP 404 from both the crates.io crate endpoint and the PyPI project endpoint on 12 July 2026. This is a point-in-time registry check, not a reservation, trademark search, company-name search, or domain clearance.

## Recommended shortlist

My preferred name is **`myrlet`**. It combines the clipped, slightly mysterious feel of `hugr` with the [Myrmidons](https://www.etymonline.com/word/myrmidon), Achilles' army traditionally associated with ants, and the diminutive `-let`: a small member of an organized army. This closely matches Hugr's model of many small specialized agents.

| Name | Why it fits | Caveat |
| --- | --- | --- |
| **`myrlet`** | Myrmidons, ants, an army, and a small unit. Suggested pronunciation: "MUR-let." | The meaning needs a one-sentence introduction. |
| **`cerelet`** | Cerebrum or cerebellum plus `-let`, giving it the sense of a small brain. | Slightly longer than `hugr`. |
| **`sootkin`** | A troop of tiny, strange helpers. Memorable and playful, closer to Bun than to enterprise AI naming. | Deliberately whimsical. |
| **`nidula`** | Latin for "little nest" and the name of a [genus of small bird's-nest fungi](https://en.wikipedia.org/wiki/Nidula). It suggests a home for small self-contained agents. | Sounds biological rather than technical. |
| **`swyra`** | An invented combination of "swarm" and "myriad." Short and apparently free of obvious existing product associations. | The project would need to establish the pronunciation, perhaps "SWY-rah." |
| **`axlet`** | Axon, axis, or axle plus `-let`, suggesting small components organized around a core. | More mechanical than biological. |
| **`hugri`** | The least disruptive successor to `hugr`, retaining almost all of its visual identity. | Less of a fresh start and not self-explanatory. |
| **`gliad`** | Glia plus myriad or triad, suggesting many supporting brain cells. | Also used by a French medical research group, though not as a package name. |
| **`coenon`** | Feels like a common organism or collective intelligence while remaining compact and neutral. | Invented and somewhat abstract. |
| **`plecta`** | Evokes plaiting, weaving, and many strands becoming one structure. | `Plectra` is already used by an AI academy, so this is not a first choice. |

The current preference order is:

1. **Myrlet**, for the closest match to the product story.
2. **Cerelet**, for the clearest small-brain association.
3. **Sootkin**, for the most memorable and characterful name.
4. **Nidula**, for the best real-word-derived name.
5. **Swyra**, for the best invented brand name.
6. **Hugri**, for the easiest migration from Hugr.

The `myrlet` package family would read naturally as `myrlet-core`, `myrlet-host`, `myrlet-agent`, `myrlet-python`, and the `myrlet` CLI.

## Additional registry-free names

The following names are also free on both crates.io and PyPI as of the check date. They have received registry checks but not full trademark or company-name clearance.

### Close to Hugr

`hugri`, `hugrin`, `hugkin`, `hugglet`, `hugling`, `hugverk`, `hugleik`, `hjarni`, `hugino`, `hugari`

### Army, colony, and assembly

`hirdr`, `enomotia`, `lochos`, `contio`, `coteri`, `posse`, `alveary`, `vespiary`, `phratry`, `sodality`, `koinon`, `symmachy`

### Mind and brain

`gliad`, `glial`, `neurlet`, `cortlet`, `noetica`, `noeton`, `nouson`, `mnemic`, `thymos`, `horme`, `atomon`, `monon`, `merion`

### Small modular things

`nodlet`, `meshlet`, `meshling`, `nexlet`, `syndet`, `cellula`, `gemmule`, `granum`, `minikin`, `holarch`, `holarchy`, `socion`, `coenobium`, `minora`

### Playful and folkloric

`greeble`, `spryte`, `pixlet`, `tinkerkin`, `motelet`, `badling`, `fluther`, `vettir`, `daktyl`, `telchine`, `diple`, `manicule`, `fleuron`, `interpunct`

### Invented swarm names

`myrva`, `myrme`, `myral`, `myrmesh`, `swarmlet`, `swarmling`, `taskling`, `cogling`, `mindling`, `fleetlet`, `rava`, `plecta`, `plekta`, `koppa`, `digamma`, `qoppa`

## Unusual options

- **`coenobium`**: a colony of cells with a fixed organization and division of labor. It is semantically close to the framework, though long.
- **`holarch`**: a component that is both an autonomous whole and part of a larger system.
- **`badling`**: an old collective term for a group of ducks. It is silly but memorable.
- **`greeble`**: a small surface detail added to a science-fiction model, which works as a metaphor for tiny specialized components.
- **`manicule`**: the old pointing-hand symbol used in manuscript margins, suggesting an agent that calls attention to something.
- **`digamma`** and **`koppa`**: archaic Greek letters that are distinctive without pretending to describe the framework.
- **`vettir`**: supernatural beings or spirits in Norse tradition, suitable for a collection of small autonomous entities.
- **`alveary`**: an old word for a beehive or repository of knowledge.

## Registry-free names to avoid

Several names are free on crates.io and PyPI but already collide directly with current AI or software products:

- `maniple`: already used by an [MCP multi-agent orchestration tool](https://pypi.org/project/maniple-mcp/).
- `myrmid`: used by an [active enterprise-agent company](https://www.myrmid.ai/en/).
- `tagma`: used by an [AI development-team product](https://www.tagma.work/).
- `munr`: used by an [infrastructure-monitoring product](https://munr.io/) with its own agent.
- `stigmerg`: close to an existing [TypeScript agent-colony framework](https://stigmergy.team/) named Stigmergy.
- `myrma`: used by an active ["AI brain" product](https://myrma.ai/) built around the colony metaphor.
- `klade`, `noetra`, `nestra`, `symploke`, `phron`, `hugra`, `huglet`, `hugora`, `formyx`, and `myria` also have meaningful contemporary AI or software collisions.

## Availability method and limitations

The check queried the exact project endpoint for each name in both registries. PyPI documents its project endpoint as `GET /pypi/<project>/json`; every suggested name returned no project. See the [PyPI JSON API documentation](https://docs.pypi.org/api/json/).

PyPI treats case and runs of hyphens, underscores, and periods as equivalent, although that does not affect the simple lowercase names used here. See the [PyPI normalization rules](https://packaging.python.org/en/latest/specifications/name-normalization/).

Registry availability can change at any time. crates.io allocates names on a first-come-first-served basis, and publishing is what secures a crate name. See the [Cargo publishing documentation](https://doc.crates.io/crates-io.html). PyPI may also withhold a deleted, prohibited, or otherwise reserved name despite the absence of a public project. An actual publishing attempt is the final registry test.

The registry checks do not constitute trademark, organization-name, social-handle, or domain clearance. Those checks should be completed before choosing the final name.
