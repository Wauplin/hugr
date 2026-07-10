#!/usr/bin/env bash
set -euo pipefail

# Builds the generic hugr-wasm crate and assembles this extension folder:
# the wasm-bindgen output goes to ./pkg and the generic JS host modules from
# bindings/typescript are vendored into ./vendor (Chrome extensions can only
# load modules from inside their own folder).

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

cargo build -p hugr-wasm --target wasm32-unknown-unknown --release
wasm-bindgen \
  --target web \
  --out-dir "$HERE/pkg" \
  target/wasm32-unknown-unknown/release/hugr_wasm.wasm

mkdir -p "$HERE/vendor"
cp "$ROOT/bindings/typescript/agent_driver.js" \
   "$ROOT/bindings/typescript/openai_adapter.js" \
   "$ROOT/bindings/typescript/indexed_db.js" \
   "$HERE/vendor/"

echo "Built $HERE/pkg and vendored the generic JS into $HERE/vendor"
