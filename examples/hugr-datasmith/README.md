# hugr-datasmith

A synthetic-data specialist: point it at any documentation folder and it returns grounded question/answer pairs, typed as a `QaDataset` (every pair cites the `source_path` that supports it). Its only tool grant is `fs_read`, jailed to that folder.

```bash
export HUGR_API_KEY=...                              # provider key for the HF router
hugr run . ../../docs "Generate 5 question/answer pairs about traces"
hugr build . --surface python --release              # also emits a typed Python wheel
```

The wheel exposes `hugr_datasmith.ask(docs_path, question) -> Answer` for calling the agent in-process from Python. [`examples/hf-librarian`](../hf-librarian) composes it into a full generate → publish → eval pipeline, walked through in [the docs-QA pipeline guide](../../docs/guides/docs-qa-dataset-pipeline.md).
