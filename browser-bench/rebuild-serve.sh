#!/usr/bin/env bash
# Reassemble the browser-bench serve dir from sources + JIT artifacts.
#
# Persistent (repo-local, gitignored) replacement for the old /tmp/out-browser
# scaffold, which a reboot wiped. Run after a reboot, a codegen change
# (rebuilds jit_codegen.wasm), or to swap the c2w artifact under test.
#
# Usage:
#   browser-bench/rebuild-serve.sh                 # uses out/alpine-node-jit7c.wasm
#   browser-bench/rebuild-serve.sh out/alpine-node-jitX.wasm
#
# Then: serve $SERVE/htdocs with COOP/COEP (see README), run collector.py +
# gen-suite.py, drive chrome-headless-shell.
set -euo pipefail
cd "$(dirname "$0")/.."   # repo root

ART="${1:-out/alpine-node-jit7c.wasm}"   # variant-4 (wasm-dispatch) c2w artifact
INTERP_ART="${2:-out/alpine-node-jit0.wasm}"  # true-interpreter baseline
SERVE="browser-bench/serve"
HT="$SERVE/htdocs"
SHIM="examples/wasi-browser/htdocs"

[ -f "$ART" ] || { echo "missing c2w artifact: $ART" >&2; exit 1; }
[ -f "$INTERP_ART" ] || { echo "missing interp artifact: $INTERP_ART" >&2; exit 1; }

mkdir -p "$HT" "$SERVE/results"

# 1. scaffold support files (browser_wasi_shim is importScripts'd source — no
#    webpack build needed). worker.js/index.html come from browser-bench, not
#    the scaffold, so they are NOT copied here.
cp -r "$SHIM/browser_wasi_shim" "$HT/"
cp "$SHIM"/{worker-util,wasi-util,stack,ws-delegate,stack-worker}.js "$HT/"
cp examples/wasi-browser/xterm-pty.conf "$SERVE/xterm-pty.conf"

# 2. bench coordinator + page (the lift/tlb/chain-aware worker.js)
cp browser-bench/htdocs/worker.js browser-bench/htdocs/index.html "$HT/"

# 3. rebuild the browser codegen wasm from the same codegen.rs the wasmtime
#    host uses (#[path] include => byte-identical block output)
cargo build --release --target wasm32-unknown-unknown \
  --manifest-path jit-codegen-wasm/Cargo.toml >/dev/null
cp jit-codegen-wasm/target/wasm32-unknown-unknown/release/jit_codegen_wasm.wasm \
  "$HT/jit_codegen.wasm"

# 4. artifacts under test
cp "$ART" "$HT/out.wasm"
cp "$INTERP_ART" "$HT/jit0.wasm"

echo "serve dir ready: $HT"
ls -1 "$HT"
