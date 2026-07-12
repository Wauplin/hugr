# huglet-weather

A tiny, self-contained Huggr weather agent. It uses only the allowlisted `web_fetch` tool (jailed to the Open-Meteo API hosts in `huggr.toml`), so there is nothing to set up beyond a provider key.

## Run it

```bash
export HUGGR_API_KEY=...            # your model provider key
huggr run . "what's the weather in Paris?"
```

The answer is the standard Huggr `Answer` JSON; `response.response` is the one-sentence weather summary.

## Next steps

- Edit `SYSTEM.md` to change the assistant's behavior or output style.
- Edit `allow_hosts` in `huggr.toml` to point `web_fetch` at other APIs.
- Adjust the response contract in `src/lib.rs` (currently a single string).
- Build a standalone binary: `huggr build . --release`.
- Inspect runs: `huggr traces .`, then `huggr replay`/`huggr verify`.
