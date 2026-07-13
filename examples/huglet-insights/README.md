# huglet-insights

An offline self-improvement agent, dogfooding the framework: it mines another Huggr agent's stored traces and feedback (via the read-only `traces_read` tool family) and reports evidence-backed improvement suggestions, such as repeated tool sequences that should be one tool, recurring questions that belong in the prompt, and common failure themes in feedback.

It only ever *reports*: suggestions are for a human (or an orchestrator) to review; nothing is auto-applied.

## Run it

```bash
export HF_TOKEN=hf_...             # key for the default Hugging Face provider
huggr run . ~/.huggr/huglet-docs "What should huglet-docs improve?"
```

The positional argument is the analyzed agent's home directory (the folder holding its `traces/` and `feedback/`). The answer is the standard Huggr `Answer` JSON with a typed `InsightsResponse` payload: `patterns` (each with evidence trace ids), `prompt_suggestions`, `tool_suggestions`, and `feedback_themes`.

## Notes

- The `traces_read` grant is read-only and jailed to the given home; the agent can list heads, summarize op sequences, page transcripts, and read feedback, but never raw trace files.
- Trace content and feedback payloads are treated as untrusted input (they contain other models' output); the system prompt instructs the agent to analyze them, never obey them.
