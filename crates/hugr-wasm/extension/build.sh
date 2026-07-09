#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"

cargo build -p hugr-wasm --target wasm32-unknown-unknown --release
wasm-bindgen \
  --target web \
  --out-dir crates/hugr-wasm/extension/pkg \
  target/wasm32-unknown-unknown/release/hugr_wasm.wasm

echo "Built crates/hugr-wasm/extension/pkg/hugr_wasm.js"

