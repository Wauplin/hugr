#!/usr/bin/env bash
set -euo pipefail

# Builds crates/huggr-wasm and emits the wasm-bindgen output (web target — Node
# initializes it from bytes, browsers from a URL) into ./pkg.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
cd "$ROOT"

cargo build -p huggr-wasm --target wasm32-unknown-unknown --release
wasm-bindgen \
  --target web \
  --out-dir "$HERE/pkg" \
  target/wasm32-unknown-unknown/release/huggr_wasm.wasm

echo "Built $HERE/pkg"
