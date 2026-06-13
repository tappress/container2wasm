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
  `cmd`/`cmd64`, `jit=off`, `chain=on`, `tlb=on`, `lift=on` (Batch 6
  register lifting — pure codegen, A/Bs on the same artifact via
  `jcg_set_lift`).
- `htdocs/index.html` — scaffold page plus bench plumbing: relays the
  worker's result JSON (`window.__c2wResult`, console `C2W_RESULT_PAGE`),
  auto-POSTs to the collector when `post=` is set, chains through
  `suite.json` when `step=` is set, watchdog for hung runs, copy button.
- `collector.py` — host-side result sink on :8081 (CORS), appends JSON
  lines to `$C2W_RESULTS_DIR/results.jsonl` (default `serve/results/`, a
  persistent non-/tmp path).
- `gen-suite.py` — emits suite.json (one query string per step).
- `rebuild-serve.sh` — assembles the persistent serve dir (below).

## Serve dir (persistent, not /tmp)

The runtime htdocs live in `browser-bench/serve/` (gitignored — it holds
~185 MB of copied wasm artifacts). The old `/tmp/out-browser` scaffold was
wiped on every reboot; `serve/` survives. `browser_wasi_shim` is plain
`importScripts`'d source, so no webpack/Docker build is needed — assembly is
just copies. Rebuild it any time (after a reboot, a codegen change, or to
swap the artifact under test):

```bash
browser-bench/rebuild-serve.sh                       # default out/alpine-node-jit7c.wasm
browser-bench/rebuild-serve.sh out/alpine-node-jitX.wasm
```

## Run

```bash
SERVE=$(pwd)/browser-bench/serve
# serve with COOP/COEP (SharedArrayBuffer) — httpd container:
docker run -d --rm --name c2w-bench -p 8080:80 \
  -v $SERVE/htdocs:/usr/local/apache2/htdocs:ro \
  -v $SERVE/xterm-pty.conf:/usr/local/apache2/conf/extra/xterm-pty.conf:ro \
  httpd:2.4 /bin/sh -c \
  'echo "Include conf/extra/xterm-pty.conf" >> /usr/local/apache2/conf/httpd.conf && httpd-foreground'

python3 browser-bench/collector.py &                 # result sink on :8081
python3 browser-bench/gen-suite.py > $SERVE/htdocs/suite.json
# full Chrome hangs under WSL2; chrome-headless-shell is real V8 and works:
CHS=~/.cache/ms-playwright/chromium_headless_shell-*/chrome-headless-shell-linux64/chrome-headless-shell
$CHS --no-sandbox --disable-gpu --disable-dev-shm-usage \
  --user-data-dir=/tmp/chs-profile \
  "http://localhost:8080/?<first step qs from gen-suite stderr>"
# the page self-chains through every suite.json step, POSTing each result.
```
