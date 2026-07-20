# Fixed datasets

Each child folder is an immutable dataset snapshot created by `hf-docs-research generate`. It contains `train.jsonl`, `test.jsonl`, and `manifest.json` with the generation traces, split seed, corpus fingerprint, source hashes, and dataset hash.

Never edit a snapshot in place. Generate a new named folder when the questions, answers, sources, split, or generation method changes. Prompt and retrieval research may inspect the train split. Keep the test split held out from variant design and use it only through the evaluator.
