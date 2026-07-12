# Naming suggestions for Hugr

This page collects possible replacements for the `hugr` project name. It favors short, memorable names connected to small specialized agents, collective intelligence, brains, mythology, or playful common-language imagery.

Every framework or standalone package candidate described as registry-free on this page was absent from both the crates.io index and the PyPI project endpoint on 12 July 2026. Component words such as “cell,” “thread,” and “tessera” are product vocabulary rather than claims of standalone package availability. This is a point-in-time registry check, not a reservation, trademark search, company-name search, or domain clearance.

## CLI-first recommendation

The CLI name is a primary constraint. It will appear in shell history, documentation, scripts, CI files, support messages, and package installation instructions more often than the ecosystem story will be explained. A strong candidate should therefore be three to five lowercase ASCII characters, one or two syllables, easy to type after hearing it once, and preferably identical to the product and root package name. Six characters can work when the word is unusually clear. Seven or more is a meaningful cost.

The related nouns should follow the same rule. An individual agent can have a short distinctive name, but ordinary concepts should remain ordinary: **tool**, **hook**, **trace**, **run**, and **group** are often better than extending a metaphor into every API. A naming system should improve prose without requiring users to translate it.

### Revised shortlist

| Product and CLI | Length | Agent | Plugin or capability | Group | Registry status | Assessment |
| --- | ---: | --- | --- | --- | --- | --- |
| **`huggr`** | 5 | huglet | hook | huddle | Free on crates.io and PyPI | The best command and vocabulary, but an existing [Huggr R project already provides Hugging Face access](https://github.com/benjaminguinaudeau/huggr). This is only viable if that exact collision can be cleared. |
| **`hugri`** | 5 | huglet | hook | huddle | Free on crates.io and PyPI | The safest direct continuation of Hugr found so far. It is quick to type and broad search found no obvious software product, but the final `i` feels appended rather than meaningful. |
| **`swyr`** | 4 | swyrl | hook | swarm | Both `swyr` and `swyrl` are free on crates.io and PyPI | Compact and visually distinctive, with no obvious software collision in a broad search. A user may not know whether to say “swire,” “swear,” or “swir.” |
| **`plect`** | 5 | strand | knot | braid | Free on crates.io and PyPI | Short, available, and built on a root meaning twisted or plaited. It gives simple component names, but `plect` is not a familiar English word. |
| **`nidu`** | 4 | nid | twig | nest | Free on crates.io and PyPI | A compact version of the nest family. It is easy to type, although pronunciation is not immediate and NIDU appears as a networking-hardware abbreviation. |
| **`merc`** | 4 | runner | wing | relay | Free on crates.io and PyPI | Preserves the Mercury messenger idea in a fast command. “Merc” also means mercenary, is a common abbreviation, and has older software uses, so searchability is weak. |

The practical recommendation is conditional:

1. **Huggr** is the strongest system if the existing Hugging Face R project can be renamed, transferred, or otherwise cleared with its author.
2. **Hugri** is the lowest-risk continuation if preserving the Hugr identity matters most.
3. **Swyr** is the cleanest four-character package candidate, although it needs an explicit pronunciation.
4. **Plect** is the strongest short name outside the Hugr family.

### The Huggr vocabulary

`huggr` is substantially better on the command line than `myrmesh`, `holobiont`, or `tessellary`. It retains the visual and phonetic identity of Hugr, is five keystrokes, and creates a compact vocabulary without obscure scientific terms.

- Product, root package, and CLI: **Huggr** and `huggr`
- One self-contained agent: a **huglet**
- A plugin or capability extension: a **hook**, while the API can continue to say “tool” or “capability”
- A configured group of cooperating agents: a **huddle**
- Package family: `huggr-core`, `huggr-host`, `huggr-agent`, `huggr-python`

The basic commands remain short and readable:

```console
huggr new weather
huggr run weather
huggr build weather
huggr traces weather
```

The exact collision matters because it is unusually close: the existing repository is named `huggr`, describes itself as a way to use the Hugging Face API from R, and imports as `library(huggr)`. It has no registry claim on crates.io or PyPI, but taking the name without coordination would create two Hugging Face-related developer projects with the same spelling.

The three-character alias `hgr` is free on crates.io and PyPI, but it should not replace `huggr` in primary documentation. It has no natural pronunciation, is used as an acronym across technical fields, and makes commands harder to repeat verbally. It could be an optional shell alias later.

### The Hg, Mercure, and Mercury family

The Mercury idea has two good stories. Mercury is the fast messenger, which suggests agents as runners and groups as relays. Elemental mercury forms droplets and amalgams, which suggests small independent units combining into a larger material.

`hg` would be an excellent two-character symbol in isolation, but it is not available in practice. [Mercurial uses `hg` as its driver command](https://mercurial-scm.org/help/topics/flags), referring to the chemical symbol for mercury, and both crates.io and PyPI already contain packages named `hg`. A new developer tool should not make `hg run` or `hg build` ambiguous beside an established version-control command.

`mercure` is also unavailable on both registries. More importantly, [Mercure is an active real-time protocol and hub](https://mercure.rocks/) that explicitly supports streaming LLM tokens, agent steps, and tool-call updates. `mercury` is heavily occupied by programming languages, developer toolkits, financial software, and current CLIs.

The shorter variants have their own problems:

- `merq` is free on crates.io and PyPI, but [Merq is an established .NET command and event bus](https://www.clarius.org/Merq/) whose name is already derived from Mercury.
- `amalg` is free on crates.io and PyPI and offers a good drop/bond/amalgam vocabulary, but it is already the executable name of a Lua module-amalgamation tool and appears in current Python development tooling.
- `merc` is the only usable short branch found in this family. Its ecosystem would be `merc` for the CLI, **runner** for an agent, **wing** for an extension, **relay** for a group, and **route** for a composition. The vocabulary is clear, but the name is generic and carries the mercenary meaning.

The Mercury exploration therefore supplies good imagery but no candidate as clean as Huggr or Hugri.

## Naming the ecosystem

A useful name should produce a vocabulary, not just a package. The framework needs a collective or habitat, each self-contained agent needs a singular noun, and tools or plugins benefit from a related term. The strongest systems also leave room for manifests, traces, shared context, and delegation without forcing every technical concept into the metaphor.

The ecosystem test is whether ordinary documentation remains clear: “create a ___,” “grant the ___ a capability,” “run the ___,” and “inspect its trace.” Cute internal names are optional. Public Rust and Python types can remain literal even if the documentation calls an agent a myrlet or a biont.

### Strongest complete systems

| Framework | Individual agent | Plugin or capability | Optional supporting vocabulary | Why it fits | Caveat |
| --- | --- | --- | --- | --- | --- |
| **`myrmesh`** | **myrlet** | trail | nest for its home, pheromone for a lightweight signal | Combines the [Myrmidons](https://www.etymonline.com/word/myrmidon), an organized army traditionally associated with ants, with a mesh of small independent units. Both `myrmesh` and `myrlet` are registry-free. | Invented, and “myr” needs a pronunciation hint. Suggested: “MEER-mesh” and “MUR-let.” |
| **`holobiont`** | **biont** | organelle | hologenome for the complete declarative configuration | A [holobiont is a whole made from distinct bionts](https://pmc.ncbi.nlm.nih.gov/articles/PMC7538806/). It is the most exact scientific analogy for self-contained specialists forming a larger functional unit, and it indirectly honors evolutionary biologist Lynn Margulis. | Nine letters, biological, and already used by unrelated health organizations. |
| **`tessellary`** | **tessera** | facet | pattern for the manifest, mosaic for a composition | A [tessera is one small piece in a mosaic](https://allaboutglass.cmog.org/definition/tessera). The agent remains legible alone but gains meaning in a composition. | `tessellary` is a coined collective, and the standalone package name `tessera` is taken even though namespaced forms remain possible. |
| **`moiron`** | **spinner** | thread | pattern for policy, shears for cancellation | Draws on the [Moirai](https://www.theoi.com/Daimon/Moirai.html), the three female Fates who spin, measure, and cut a life’s thread. It gives the system a coherent language for orchestration and deterministic execution. | The fate metaphor can feel ominous, and `moira` itself is already taken on both registries. |
| **`coenobium`** | **cell** | organelle | matrix for shared context | A [coenobium is a colony whose cells have a fixed arrangement](https://www.merriam-webster.com/dictionary/coenobium). The distinction between the colony and one cell maps directly to framework and agent. | Harder to spell and pronounce, and biological coenobia do not always imply specialized cells. |
| **`volarium`** | **bird** | feather | nest for configuration, flock for a running group, flight for delegation | Develops the bird idea into simple language: build a bird, equip it with feathers, and run a flock in Volarium. The name is a deliberate coinage influenced by *volary*, [a word for an aviary or its flock](https://www.merriam-webster.com/dictionary/volary). | It names the habitat more naturally than the collective, and the real word `volary` cannot be used safely because it is already an AI-agent product name. |
| **`sootkin`** | **mote** | spark | hearth for shared state | Feels like a family of tiny animated helpers. It has the common-word charm of Bun and a light connection to soot sprites without taking a character name. | Whimsical and less suitable if the project wants a serious scientific tone. |
| **`nidula`** | **nestling** | twig | nest for the manifest, flight for delegation | Uses *Nidula*, Latin for “little nest” and a [genus of bird’s-nest fungi](https://en.wikipedia.org/wiki/Nidula), as the home of small agents assembled from declared pieces. Both `nidula` and `nestling` are registry-free. | A nest is a home rather than a collective, and “nestling” suggests an immature agent. |

### Best ecosystem semantics: Myrmesh

When only the whole-to-part metaphor is considered, **Myrmesh** is the strongest system and Myrlet is best used for the individual artifact. With CLI length included, `myrmesh` is too long to remain the overall recommendation.

- Product and CLI: `myrmesh`
- Self-contained agent: a **myrlet**
- Capability or plugin: a **trail**, when a metaphorical term is useful
- Agent home and manifest context: a **nest**
- Package family: `myrmesh-core`, `myrmesh-host`, `myrmesh-agent`, `myrmesh-python`

The resulting prose is reasonably natural: “Create a myrlet from a manifest,” “grant the myrlet filesystem and web trails,” “run the myrlet with Myrmesh,” and “compose several myrlets as tools.” The last sentence can stay literal where “trail” would obscure what the API actually does.

### Best science system: Holobiont

**Holobiont** has the strongest real scientific whole-to-part relationship. A biont is an individual organism; the holobiont is the compound unit, and its hologenome is the combined genetic information. The analogy gives a clean hierarchy without implying that the individual agent stops being autonomous.

- Product and CLI: `holobiont`
- Self-contained agent: a **biont**
- Plugin or capability: an **organelle**
- Complete declarative configuration: the **hologenome**
- Package family: `holobiont-core`, `holobiont-host`, `holobiont-agent`, `holobiont-python`

This also provides the best historical connection. Lynn Margulis developed symbiogenesis into a central account of how complex cells arose through cooperation among once-independent organisms. The name acknowledges the scientific idea rather than placing a person’s surname directly on the software.

### Best visual system: Tessellary

**Tessellary** makes each agent a tessera and the larger application a mosaic. It is less army-like than Myrmesh and less organic than Holobiont, but it describes specialization well: each piece may have a different shape and purpose while sharing a narrow boundary with its neighbors.

- Product and CLI: `tessellary`
- Self-contained agent: a **tessera**
- Plugin or capability: a **facet**
- Manifest or composition declaration: a **pattern**
- Multi-agent application: a **mosaic**

### More ecosystem sketches

These are useful secondary directions. The framework names are registry-free, but the component words are intended as product vocabulary and may already exist as standalone package names.

| Theme | Framework | Agent | Plugin or capability | Other vocabulary | Comment |
| --- | --- | --- | --- | --- | --- |
| Greek civic alliance | `symmachy` | ally | grant | compact, council | A real word for an alliance, suitable for independent agents with bounded privileges. |
| Greek common body | `koinon` | member | office | charter, assembly | Historically a political or religious common organization. Short, but close to “coin-on” in English. |
| Roman assembly | `comitia` | delegate | mandate | forum, decree | Clear collective structure with a strong antiquity connection. More political than technical. |
| Fellowship | `sodality` | fellow | craft | rule, chapter | A real association organized around a purpose. Long, but readable. |
| Guild | `guildhall` | artisan | tool | charter, workshop | Makes specialization explicit and keeps “tool” literal. The framework name is generic. |
| Commons | `commonweal` | steward | grant | charter, commons | Inspired by [Elinor Ostrom’s work on governance of shared resources](https://www.nobelprize.org/prizes/economic-sciences/2009/ostrom/facts/). It suits agents sharing state and capabilities under explicit rules. |
| Fire spirits | `lampades` | lampad | spark | torch, ember | The Lampades are female torch-bearing nymphs associated with Hecate. Distinctive, although games already use the mythological name. |
| Constellation | `sidereum` | star | ray | orbit, chart | Each specialist is a point with its own identity; a chart declares their relationships. Less grounded in the framework’s execution model. |
| Mycelial network | `hypharium` | hypha | branch | mycelium, spore | A living network assembled from narrow strands. “Hypha” is less approachable than “myrlet” or “biont.” |
| Coral colony | `atollery` | polyp | branch | reef, lagoon | Specialized small organisms build a persistent larger structure. The coined framework name sounds more like a place than a runtime. |
| Music | `choirlet` | voice | part | score, chorus | A score coordinates specialized voices. The framework name sounds like the individual rather than the ensemble. |
| Weaving | `plecton` | strand | knot | loom, pattern | Derived from Greek roots for plaiting or weaving. It is compact but needs explanation. |
| Science-fiction modelmaking | `greeblery` | greeble | panel | hull, kit | [Greebles are small model details associated with the production of *Star Wars*](https://www.cabinetmagazine.org/issues/63/burnett.php). A greeblery would be a system whose tiny purpose-built additions make the whole useful and legible. |
| Detective fiction | `irregulars` | scout | clue | casebook, dispatch | The [Baker Street Irregulars served as Holmes’s distributed street agents](https://collections.libraries.indiana.edu/lilly/exhibitions/exhibits/show/history-of-bsi/in-the-beginning). The analogy is unusually close, but the name remains strongly tied to Sherlock Holmes adaptations and societies. |
| Folklore | `sootkin` | mote | spark | hearth, troop | The strongest playful system and the closest to a Bun-like naming move. |
| Duck collective | `badling` | duck | feather | nest, flight | “A badling of ducks” is memorable and absurd. Better as a mascot-rich project than a neutral infrastructure name. |
| Jellyfish collective | `fluther` | jelly | tentacle | current, bloom | Another unusual collective noun with good visual potential, but “jelly” is weak technical vocabulary. |

### Names inspired by women in science and history

Direct surname names can work, but they describe a tribute rather than an ecosystem. These were also absent from crates.io and PyPI on the check date. They are strongest when the person’s work supplies the surrounding vocabulary.

| Framework | Agent | Plugin or capability | Inspiration | Caveat |
| --- | --- | --- | --- | --- |
| **`bartik`** | operator | cord or patch | [Jean Bartik and the women who programmed ENIAC](https://eniacprogrammers.org/documentary-info/), where a team of specialists configured one machine. | “Operator” understates their programming work, and the surname does not denote a collective. |
| **`ostrom`** | steward | grant | Elinor Ostrom’s study of communities governing shared resources through explicit local rules. | Governance is a strong policy metaphor but a weak name for the agent itself. |
| **`kwolek`** | fiber | weave | [Stephanie Kwolek’s discovery of Kevlar fiber](https://www.invent.org/inductees/stephanie-louise-kwolek), with many light strands forming a strong material. | The analogy describes composition better than agent behavior. |
| **`merian`** | imago | wing | Maria Sibylla Merian’s study and illustration of insect metamorphosis. | “Imago” implies a final life stage rather than a small specialist. |
| **`maathai`** | tree | seed | [Wangari Maathai and the Green Belt Movement](https://www.nobelprize.org/prizes/peace/2004/maathai/facts/), where local planting formed a large distributed effort. | The tree metaphor suggests growth and persistence more than delegation. |
| **`vaughan`** | computer | trajectory | [Dorothy Vaughan’s leadership of NACA’s West Area Computing unit](https://www.nasa.gov/people/dorothy-vaughan/), a team of specialized human computers. | “Computer” now means the host machine, so the historical vocabulary would confuse API prose. |
| **`mirzakhani`** | surface | curve | Maryam Mirzakhani’s work on the geometry and dynamics of curved surfaces. | Beautiful visual territory, but long and not naturally collective. |
| **`yonath`** | subunit | codon | Ada Yonath’s work on the structure and function of the ribosome. | Biologically precise but harder to turn into friendly product language. |
| **`herschel`** | comet | orbit | Caroline Herschel’s astronomical discoveries and systematic observational work. | Widely used as a surname and institutional name outside package registries. |

Of the historical directions, **Holobiont** is stronger than using `margulis` directly, and **Commonweal** is stronger ecosystem language than using `ostrom` directly. **Bartik** is the best direct surname if the project wants to foreground the history of small specialist programs working as one system.

### Ecosystem names rejected after broader screening

These names were absent from crates.io and PyPI, but their existing uses are too close to the intended project:

- `volary`: an exact [memory platform for AI agents](https://volary.ai/), despite being the most tempting real bird-collective word.
- `motet`: an existing [multi-agent runtime](https://motet.dev/), despite its excellent “small voices following a score” vocabulary.
- `thriae`: already used for an [autonomous swarm of space drones](https://isp-space.com/thriae/), almost the same story as a framework of specialized agents.
- `pyrosome`: already used by a [verified compilation framework](https://arxiv.org/abs/2507.06360) and a [microservices runtime](https://openreview.net/pdf?id=HDXGk6PkIl).
- `formicary`: already used by a [distributed orchestration engine](https://weblog.plexobject.com/archives/category/go-2) and now reused in agent-workflow discussions.
- `knotwork`: already used by an [AI expertise-matching product](https://apps.apple.com/us/app/knotwork/id6761410191) and in current AI research terminology.
- `syntrophy`: already used by a [technology company](https://www.syntrophy.tech/) and a nearby [AI company](https://www.syntrophic.ai/).
- `pleiad`: used by software research projects and an earlier agent framework.
- `sibyllae`: used as the plural name for assistants in an existing multi-assistant AI system.
- `lamarr`: used by a simulation framework, AI companies, and a research institute working on multi-agent systems.
- `aviarium`: used by a game and an AI organization, while `ornithon` is used by poultry-management software.

## Earlier standalone-name shortlist

Before applying the CLI constraint, the semantic-only shortlist preferred **`myrlet`**. It combines the clipped, slightly mysterious feel of `hugr` with the [Myrmidons](https://www.etymonline.com/word/myrmidon), Achilles' army traditionally associated with ants, and the diminutive `-let`: a small member of an organized army. It now works better as the individual agent term under Myrmesh than as the product command.

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

The earlier preference order for a single project name, without considering ecosystem vocabulary or CLI length, was:

1. **Myrlet**, for the closest match to the product story.
2. **Cerelet**, for the clearest small-brain association.
3. **Sootkin**, for the most memorable and characterful name.
4. **Nidula**, for the best real-word-derived name.
5. **Swyra**, for the best invented brand name.
6. **Hugri**, for the easiest migration from Hugr.

The package family reads naturally, but `myrlet` is six characters and `myrmesh` is seven. Both lose to the CLI-first candidates for repeated terminal use.

## Additional registry-free names

The following names are also free on both crates.io and PyPI as of the check date. They have received registry checks but not full trademark or company-name clearance.

### Close to Hugr

`hugri`, `hugrin`, `hugkin`, `hugglet`, `hugling`, `hugverk`, `hugleik`, `hjarni`, `hugino`, `hugari`, `hugz`, `hugx`, `huga`, `hgur`

### Army, colony, and assembly

`hirdr`, `enomotia`, `lochos`, `contio`, `coteri`, `posse`, `alveary`, `vespiary`, `phratry`, `sodality`, `koinon`, `symmachy`

### Mind and brain

`gliad`, `glial`, `neurlet`, `cortlet`, `noetica`, `noeton`, `nouson`, `mnemic`, `thymos`, `horme`, `atomon`, `monon`, `merion`

### Small modular things

`nodlet`, `meshlet`, `meshling`, `nexlet`, `syndet`, `cellula`, `gemmule`, `granum`, `minikin`, `holarch`, `holarchy`, `socion`, `coenobium`, `minora`

### Playful and folkloric

`greeble`, `spryte`, `pixlet`, `tinkerkin`, `motelet`, `badling`, `fluther`, `vettir`, `daktyl`, `telchine`, `diple`, `manicule`, `fleuron`, `interpunct`

### Invented swarm names

`myrva`, `myrme`, `myral`, `myrmesh`, `swyr`, `swyrl`, `swarmlet`, `swarmling`, `taskling`, `cogling`, `mindling`, `fleetlet`, `rava`, `plect`, `plecta`, `plekta`, `koppa`, `digamma`, `qoppa`

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

The check queried the exact sparse-index path for each crate name and the exact project endpoint for each Python name. PyPI documents its project endpoint as `GET /pypi/<project>/json`; every registry-free candidate returned no project. See the [PyPI JSON API documentation](https://docs.pypi.org/api/json/).

PyPI treats case and runs of hyphens, underscores, and periods as equivalent, although that does not affect the simple lowercase names used here. See the [PyPI normalization rules](https://packaging.python.org/en/latest/specifications/name-normalization/).

Registry availability can change at any time. crates.io allocates names on a first-come-first-served basis, and publishing is what secures a crate name. See the [Cargo publishing documentation](https://doc.crates.io/crates-io.html). PyPI may also withhold a deleted, prohibited, or otherwise reserved name despite the absence of a public project. An actual publishing attempt is the final registry test.

The registry checks do not constitute trademark, organization-name, social-handle, or domain clearance. Those checks should be completed before choosing the final name.
