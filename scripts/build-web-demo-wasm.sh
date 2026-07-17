#!/usr/bin/env bash
set -euo pipefail

repo_dir="$(cd "$(dirname "$0")/.." && pwd)"

if [[ -n "${EMSDK:-}" && -f "${EMSDK}/emsdk_env.sh" ]]; then
  # shellcheck disable=SC1091
  source "${EMSDK}/emsdk_env.sh" >/dev/null
elif ! command -v emcc >/dev/null 2>&1; then
  echo "Set EMSDK to an Emscripten SDK, or put emcc on PATH." >&2
  exit 1
fi

emscripten_major="$(emcc --version | sed -nE '1s/.* ([0-9]+)\..*/\1/p')"
if [[ -z "$emscripten_major" || "$emscripten_major" -lt 6 ]]; then
  echo "Graft Web Workbench requires Emscripten 6 or newer (found $(emcc --version | head -1))." >&2
  exit 1
fi

export RUSTFLAGS="-C opt-level=1 \
-C link-arg=-sWASMFS \
-C link-arg=-O2 \
-C link-arg=-lopfs.js \
-C link-arg=-sMODULARIZE=1 \
-C link-arg=-sEXPORT_ES6=1 \
-C link-arg=-sEXPORT_NAME=createGraft \
-C link-arg=-sINVOKE_RUN=0 \
-C link-arg=-sEXIT_RUNTIME=0 \
-C link-arg=-sASYNCIFY=1 \
-C link-arg=-sASYNCIFY_STACK_SIZE=1048576 \
-C link-arg=-sSTACK_SIZE=1048576 \
-C link-arg=-sEXPORTED_RUNTIME_METHODS=callMain,stackSave,stackRestore,Asyncify \
-C link-arg=-sALLOW_MEMORY_GROWTH=1"

cd "$repo_dir"
cargo build -p graft-tool --target wasm32-unknown-emscripten --release

mkdir -p web-demo/public/wasm
cp target/wasm32-unknown-emscripten/release/graft.js web-demo/public/wasm/graft.js
cp target/wasm32-unknown-emscripten/release/graft.wasm web-demo/public/wasm/graft.wasm

echo "Wrote web-demo/public/wasm/graft.js and graft.wasm"
