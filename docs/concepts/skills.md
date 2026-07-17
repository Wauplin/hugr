# Skills

This page explains how a huglet uses Agent Skills: what a skill folder looks like, how progressive disclosure keeps skills out of the context until needed, how to bundle skills with an agent definition versus attaching them at invocation time, and where the trust boundary sits. It applies to manifest-defined agents, the built CLI binary, MCP, and the Python runtime API.

## The problem

Some instructions are too long or too situational to live in `SYSTEM.md`. A citation format, a review checklist, a step-by-step procedure for one kind of task: baking them all into the system prompt makes every ask pay their token cost, whether or not the task needs them. Splitting them into separate agents is too heavy when all that differs is instructions.

Skills solve this with on-demand instructions. The model always knows *which* skills exist, and loads *what* a skill says only when the task calls for it.

## The skill format

Huggr uses the standard Agent Skills folder format: a folder named for the skill containing a `SKILL.md` with YAML frontmatter followed by Markdown instructions. Reference files, scripts, and assets sit beside it.

```
skills/source-citation/
  SKILL.md
  references/
    style.md
```

```markdown
---
name: source-citation
description: Cite documentation sources. Use when the answer quotes or relies on a docs file.
---

When answering from documentation, cite every source file...
```

The frontmatter has exactly two required fields, and validation is strict:

- `name` must equal the folder name, be 1 to 64 bytes, and use only lowercase ASCII letters, digits, and single hyphens (no leading, trailing, or doubled hyphen).
- `description` must be 1 to 1024 bytes. It is the only text the model sees before loading the skill, so state both what the skill does and when to use it.

Names must be unique across everything attached to one ask. A duplicate, an unreadable path, or invalid frontmatter fails the ask up front with a specific error rather than silently dropping the skill.

## Progressive disclosure

Discovered skills do not enter the context as instructions. The system prompt gains one catalog section listing each skill as `` `name`: description ``, plus a fixed instruction to call `skill_read` before acting when a skill matches the task. The bodies stay on disk.

`skill_read` is registered automatically whenever at least one skill is attached; it is not a manifest grant. It takes the skill `name` and an optional relative `path` that defaults to `SKILL.md`, so the model first loads the instructions and then, only if they reference other files, reads those with the same tool.

A skill therefore costs one catalog line per ask until it is used, and one tool call plus its file contents when it is. Deep material belongs in referenced files, not in `SKILL.md` itself, so even a used skill loads only what the task needs.

## Definition skills versus runtime skills

Skills attach in two ways, and both go through the same validation and disclosure path.

**Definition-owned skills** ship with the agent. The manifest declares folder paths relative to the agent crate:

```toml
skills = ["skills/source-citation", "../shared-skills"]
```

Each entry may point at one skill folder, a `SKILL.md` file directly, or a directory whose immediate child directories are each a skill. `huggr build` bundles them into the artifact, so a built binary carries its skills.

**Runtime skills** are attached to a single invocation by the caller, with paths resolved from the caller's working directory:

```bash
my-agent --skill ./skills/incident-runbook "Summarize yesterday's incidents"
```

The flag is repeatable. The same field exists on `Ask.skills` in Rust, the `skills` array on the MCP `ask` tool, and `skills=` in the generated and runtime Python APIs. The TypeScript contract type mirrors the field, but its `Agent.ask` and `Agent.run` options do not currently accept skills. Runtime and definition skills are merged before discovery, so a runtime skill whose name collides with a bundled one is an error, not an override.

Use definition skills for instructions the agent always might need; use runtime skills when the caller owns the procedure, for example an orchestrator that hands each specialist the runbook for the current job.

## The trust model

Skill instructions are trusted prompt input, on the same footing as `SYSTEM.md`: they can steer the model but cannot register capabilities or widen any jail. `skill_read` itself is jailed to the selected skill folder, rejects traversal and symlink escapes after canonicalization, reads only UTF-8 files, and caps each file at 1 MB.

The corollary is about who supplies the path. Whoever controls a skill folder controls part of the ask's instructions, so accepting a runtime skill path from an untrusted party hands them the prompt. The [security notes](security.md) state this as the rule: do not accept runtime skill paths unless granting that party control over the instructions is intended.

## Worked example

A documentation agent can bundle a citation skill:

```toml
skills = ["skills/source-citation"]
```

On an ask, the system prompt lists `source-citation` with its description. A question that quotes documentation can make the model call `skill_read {"name": "source-citation"}`, follow the returned citation rules, and answer with sources; a question the skill does not match need not load it. In the trace, the `skill_read` call and its result are ordinary tool records, so `huggr replay` shows exactly which instructions the model was following.

## Limitations

- Disclosure depends on the description. A vague description means the model either never loads the skill or loads it constantly; write descriptions as matching rules, not summaries.
- Skills carry instructions only. A skill that needs a tool the manifest does not grant cannot add it; scripts beside a skill are only useful to an agent that already has a way to run them.
- Skill files must be UTF-8 text and at most 1 MB each; binary assets cannot be read through `skill_read`.
- Names are validated but not namespaced. Two sources that both ship a `review` skill cannot be attached to the same ask.
