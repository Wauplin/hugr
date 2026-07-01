#!/usr/bin/env bash
# Build the baton-wasm module and drop the JS glue + .wasm into the extension.
#
# Prereqs (one-time):
#   rustup target add wasm32-unknown-unknown
#   cargo install wasm-bindgen-cli --version 0.2.100   # must match the crate's wasm-bindgen dep
#
# Usage:  ./crates/baton-wasm/build-extension.sh
# Then load `crates/baton-wasm/extension/` as an unpacked extension in Chrome.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
out="$here/extension/wasm"

echo "▸ compiling baton-wasm → wasm32-unknown-unknown (release)"
cargo build -p baton-wasm --target wasm32-unknown-unknown --release --manifest-path "$root/Cargo.toml"

wasm="$root/target/wasm32-unknown-unknown/release/baton_wasm.wasm"

echo "▸ generating JS glue with wasm-bindgen → $out"
mkdir -p "$out"
wasm-bindgen "$wasm" --out-dir "$out" --target web --no-typescript

# Optional: shrink further if wasm-opt (from binaryen) is on PATH.
if command -v wasm-opt >/dev/null 2>&1; then
  echo "▸ wasm-opt -Oz"
  wasm-opt -Oz "$out/baton_wasm_bg.wasm" -o "$out/baton_wasm_bg.wasm"
fi

echo "✓ done. Size:"
ls -lh "$out/baton_wasm_bg.wasm" | awk '{print "  " $5 "  " $9}'
