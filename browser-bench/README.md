# Browser JIT benchmark harness

Browser-side counterpart of the `jit-host/` wasmtime embedder: runs a
JIT_DISPATCH_VARIANT=4 c2w artifact in a worker with a JS coordinator
providing the `jit.*` imports, and measures boot-to-exit wallclock per
container command. See CLAUDE.md "Browser-port checkpoint" for results.

## Pieces

- `htdocs/worker.js` — scaffold worker plus the JIT coordinator:
  `register_block2` reads IR from guest memory, calls the
  `jit-codegen-wasm` module (same codegen.rs as jit-host, byte-identical
  output), sync-compiles via `new WebAssembly.Module`, instantiates against
  the guest's memory/table/helpers, `table.grow`+`set`, returns the slot.
  Content cache keyed on (pc, end_pc, n_insns, IR bytes). flush/mark are
  stats-only (invalidation is generation-checked guest-side). URL params:
  `cmd`/`cmd64`, `jit=off`, `chain=on`, `tlb=on`.
- `htdocs/index.html` — scaffold page plus bench plumbing: relays the
  worker's result JSON (`window.__c2wResult`, console `C2W_RESULT_PAGE`),
  auto-POSTs to the collector when `post=` is set, chains through
  `suite.json` when `step=` is set, watchdog for hung runs, copy button.
- `collector.py` — host-side result sink on :8081 (CORS), appends JSON
  lines to /tmp/c2w-results/results.jsonl.
- `gen-suite.py` — emits suite.json (one query string per step).

## Setup

The remaining htdocs files come from `examples/wasi-browser` (plus the
webpack-built browser_wasi_shim bundle from its Dockerfile) and the
`jit-codegen-wasm` crate build:

```bash
cd examples/wasi-browser && docker build --output=$HTDOCS .
cp examples/wasi-browser/htdocs/{worker-util,wasi-util,stack,ws-delegate,stack-worker}.js $HTDOCS/
cp browser-bench/htdocs/* $HTDOCS/
cargo build --release --target wasm32-unknown-unknown \
  --manifest-path jit-codegen-wasm/Cargo.toml
cp jit-codegen-wasm/target/wasm32-unknown-unknown/release/jit_codegen_wasm.wasm \
  $HTDOCS/jit_codegen.wasm
cp out/alpine-node-jit7c.wasm $HTDOCS/out.wasm     # variant-4 artifact
cp out/alpine-node-jit0.wasm $HTDOCS/jit0.wasm     # true-interp baseline
# serve $HTDOCS with COOP/COEP (examples/wasi-browser/xterm-pty.conf), then:
python3 browser-bench/collector.py &
python3 browser-bench/gen-suite.py > $HTDOCS/suite.json
chrome-headless-shell --no-sandbox "http://localhost:8080/?<first step qs>"
```
