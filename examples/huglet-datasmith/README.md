# huglet-datasmith

A synthetic-data specialist: point it at any documentation folder and it returns grounded question/answer pairs, typed as a `QaDataset` (every pair cites the `source_path` that supports it). Its only tool grant is `fs_read`, jailed to that folder.

```bash
export HF_TOKEN=hf_...                               # key for the default Hugging Face provider
huggr run . ../../docs "Generate 5 question/answer pairs about traces"
huggr build . --surface python --release              # also emits a typed Python wheel
```

The wheel exposes `huglet_datasmith.ask(docs_path, question) -> Answer` for calling the agent in-process from Python. [`examples/hf-librarian`](../hf-librarian) composes it into a full generate → publish → eval pipeline, walked through in [the docs-QA pipeline tutorial](../../docs/tutorials/docs-qa-dataset-pipeline.md).
