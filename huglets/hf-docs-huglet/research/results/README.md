# Evaluation results

The evaluator writes one append-only JSON file per variant run. Each file records the dataset hash, documentation fingerprint and drift, Git state, candidate and judge models, per-case traces, answer quality, citation recall, latency, tokens, tool calls, and separate candidate and judge costs.

Do not overwrite a run. Use `hf-docs-research report` to rebuild `charts/summary.csv`, `charts/overview.png`, and `charts/quality-cost.png` from every result JSON in this folder.
