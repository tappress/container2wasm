# Project context

This is a fork of container2wasm. The user is exploring it as a substrate for a **browser-side Linux sandbox** for web AI agents — letting them run arbitrary Python/Node.js, shell tools, and (long-term) container workloads in-browser, with iframe-to-iframe network emulation so an AI-generated UI in one frame can call a backend served from the sandbox.

Background: the user previously tried QEMU+x86 with syscall emulation and found it too slow (futex stalls, syscall-emulation overhead). RISC-V via TinyEMU (c2w's path) is the current direction.

## Exploration paths

Two concrete paths under consideration. **Path 2 is the current direction.**

### Path 2 (current) — Full riscv64 general-purpose Linux

One persistent VM, real distro rootfs (Debian/Alpine), Node + dev tooling installed inside. Goal: run coding agents (Claude Code, Codex) that shell out to `cp`, `git`, `rg`, spawn subprocesses, write files. Persistent disk backed by IndexedDB/OPFS so state survives reload.

Known risks to measure early:
- **fork/exec cost** through emulated kernel — likely the dominant perf factor for agent workloads
- **Memory ceiling** — Debian + Node easily 500MB-1GB; needs WASM memory64
- **Persistent FS** — not built into c2w, needs an IndexedDB/OPFS-backed block device
- **Outbound network** — API calls via c2w-net WS bridge or direct CORS

If raw fork/exec turn time is unbearable here, the whole full-kernel approach is wrong for this workload and we should look at WASIX/wasm-native runtimes instead.

### Path 1 (fallback / differentiator slice) — Thin iframe-networking slice

One RISC-V c2w VM running e.g. a Python `http.server`, an iframe with AI-generated HTML hitting it over a MessageChannel/SharedArrayBuffer-backed transport (instead of c2w-net's default WebSocket bridge). Validates the most novel piece: cross-iframe socket emulation, which nothing on the market (WebContainer, v86) currently does.

Worth building either as a feasibility probe before committing to Path 2, or as the headline demo once Path 2 works.

## TinyEMU fork & dev iteration loop

We forked TinyEMU at [tappress/tinyemu-c2w](https://github.com/tappress/tinyemu-c2w) (clone at `/home/and/Projects/tinyemu-c2w/`) so we can patch the emulator directly. Fork is based on ktock's c2w patch set (commit `e4e9bd1` on top of Bellard's import) — that's the version c2w expects.

### Build modes

**Production / matches upstream:** edit the pinned commit in [Dockerfile](Dockerfile) lines 32-33, then normal `./out/c2w --target-arch=riscv64 ... alpine:3.20 ./out/alpine-rv64.wasm`. Requires push to remote.

**Dev / picks up uncommitted working-tree changes:** use [Dockerfile.local](Dockerfile.local) — it replaces the `git clone` of TinyEMU with a buildx `--build-context tinyemu-local=...` COPY, and adds ccache + a persistent buildkit cache mount for `make`. Command:

```bash
./out/c2w \
  --target-arch=riscv64 \
  --dockerfile ./Dockerfile.local \
  --extra-flag --build-context=tinyemu-local=/home/and/Projects/tinyemu-c2w \
  --build-arg OPTIMIZATION_MODE=native \
  alpine:3.20 ./out/alpine-local.wasm \
  && cp out/alpine-local.wasm /tmp/out-browser/htdocs/out.wasm
```

`OPTIMIZATION_MODE=native` skips the wizer pre-init step — saves ~4s per rebuild at the cost of slower first boot at runtime. Fine for dev iteration. For perf measurement and release builds, drop that flag (default is `wizer`).

### Iteration times (measured)

| Scenario | Time |
|---|---|
| Cold rebuild (fresh layer cache) | ~10 min (wizer) / ~7 min (native) |
| No-source-change rebuild | ~4s (everything CACHED) |
| 1-line C change, wizer mode | 21s |
| 1-line C change, native mode | **13s** |

Where the 13s native-mode time goes: ~4s buildkit setup, ~0.5s `make` (ccache hit), ~2.8s wasi-vfs pack, ~5s misc small steps. ccache is the reason `make` is sub-second — without it, full recompile of TinyEMU's ~37k LOC would dominate.

### Browser serving

Httpd container `c2w-tappress` on port 8080 serves [/tmp/out-browser/htdocs/](file:///tmp/out-browser/htdocs/) with COOP/COEP headers (required for SharedArrayBuffer). Started with:

```bash
docker run -d --rm --name c2w-tappress -p 8080:80 \
  -v /tmp/out-browser/htdocs:/usr/local/apache2/htdocs:ro \
  -v /tmp/out-browser/xterm-pty.conf:/usr/local/apache2/conf/extra/xterm-pty.conf:ro \
  httpd:2.4 \
  /bin/sh -c 'echo "Include conf/extra/xterm-pty.conf" >> /usr/local/apache2/conf/httpd.conf && httpd-foreground'
```

Page lives at http://localhost:8080/. The browser scaffold was assembled earlier from c2w's `examples/wasi-browser` plus `c2w-net-proxy.wasm`.

### When `--external-bundle` would help (not yet enabled)

`--external-bundle` keeps the OCI container rootfs *outside* the .wasm and mounts it at runtime ([Dockerfile.local:113-118](Dockerfile.local#L113-L118), driven by c2w flag in [cmd/c2w/main.go:239](cmd/c2w/main.go#L239)). For our current Alpine dev build it'd save ~1s of wasi-vfs pack — not worth it.

It becomes worthwhile when:
- **Heavier base images** (Debian + node baked in was ~125MB last session) — pack time scales with rootfs size, so the saving grows proportionally.
- **Swapping containers without rebuilding TinyEMU** — wasm stays cached, only the bundle file changes. Big iteration win when testing different agent containers against the same emulator.

When you flip to it, the wasm output becomes a .tar.gz with the bundle as a separate file; the browser scaffold needs to mount it via WASI preopens before `_start`.

## Where we are in the Tier 1 / Tier 2 / Tier 3 roadmap

Recap of why the fork exists: V8 won't run with JIT under either TinyEMU (SIGILL on missing ISA op) or QEMU-Wasm (SEGV from fence.i / SMC handling). qemu-user-static differential test confirmed the riscv64 node binary is fine — both browser emulators have gaps. `node --jitless` works under QEMU-Wasm but at 22s for hello-world; full Linux + Node native turn time appears bounded at ~250-300x slower than host (≈486-class effective CPU on AMD Ryzen 9 7000 host).

Tier plan:
- **Tier 1** ✅ done (2026-05-23): The single blocker was **FENCE.TSO** (`fm=8` variant of FENCE), which V8 emits unconditionally. TinyEMU's FENCE decoder at [tinyemu-c2w/riscv_cpu_template.h:1351](../tinyemu-c2w/riscv_cpu_template.h#L1351) rejected any non-zero `fm` field; kernel can't synthesize FENCE in its trap handler → SIGILL forwarded to userland → Node died silently. Two commits in [tappress/tinyemu-c2w](https://github.com/tappress/tinyemu-c2w):
  - [`4093c10`](https://github.com/tappress/tinyemu-c2w/commit/4093c10) — the actual blocker fix. Relax FENCE validation mask `0xf00fff80` → `0x000fff80` (only require rs1=rd=0, ignore fm). RV spec permits this (reserved FENCE encodings are implementation-defined and may be conservatively treated as a global FENCE; on a uniprocessor any FENCE is a no-op).
  - [`d53620f`](https://github.com/tappress/tinyemu-c2w/commit/d53620f) — perf cleanup that follows. Implement `utime` CSR (0xC01) in TinyEMU's `csr_read` returning `insn_counter / 16` (matches RTC_FREQ_DIV; consistent with the mtime MMIO read and the `timebase-frequency` device-tree value). Also add PMP CSR stubs (0x3A0-0x3EF) so OpenSBI's M-mode setup writes don't trap. After this, alpine+node boot + `node -e` runs produce **zero illegal-instruction traps** end-to-end.

  No other missing instructions surfaced. Node runs to completion both headless (wasmtime CLI) and in-browser (xterm-pty scaffold on :8080).
- **Tier 2** (2-3 weeks each): Browser-integration speedups — virtio-blk backed by OPFS for persistent disk, virtio-9p for host-FS sharing, virtio-vsock for iframe-iframe sockets.
- **Tier 3** (2-4 months, big lift): Add a WASM-emitting JIT inside the WASM emulator. Skip if Tier 1+2 produce acceptable perf for the agent workload.

### Tier 1 verification & perf baseline (wasmtime CLI, 2026-05-23)

CLI repro requires `--no-stdin` (wasmtime's WASI poll_oneoff errors on non-TTY stdin with "Inappropriate ioctl for device"):

```bash
wasmtime run ./out/alpine-node.wasm --no-stdin node -e 'console.log(1+1)'
```

The `alpine-node-rv64:local` image is built from `alpine:edge + apk add nodejs` (Node 24.15.0). Convert with the same `Dockerfile.local` + `--build-context tinyemu-local=...` dev loop above.

Stress test (boot + 3 node invocations, total ≈ 10s wallclock on a Ryzen 9 7000):
- 1000-element `Array.reduce` + JSON.stringify: 5.9s end-to-end, output correct
- 1M-iteration arithmetic loop: **2.07s inside guest = ~483k iter/sec**, ≈1000× slower than native for this trivial loop
- 5000-object `JSON.stringify` + parse + reduce: 9.8s end-to-end, output correct

This perf is **borderline-usable for small agent shell-outs** (single `git status`, `rg`, short node script) but **punishing for tight iteration**. The 1000× ratio matches the upper end of CLAUDE.md's prior 250-300× estimate; the gap is plausibly the unmeasured cost of frequent illegal-insn traps (Node still triggers ~16k `csrr time` traps per run, each going through Linux's user-mode CSR emulator). Measure fork/exec turn time on a realistic agent workload before deciding whether to invest in Path 2 vs WASIX pivot.

### Diagnostic instrumentation (working-tree only, not committed)

[tinyemu-c2w/riscv_cpu_template.h](../tinyemu-c2w/riscv_cpu_template.h) has a working-tree-only printf hook at the `illegal_insn:` label that logs first-occurrence (pc, insn, priv) tuples with a hit-count, capped at 512 unique entries. Keep it during further ISA exploration; strip before any commit. The fork's main branch contains only the FENCE.TSO fix on top of ktock's c2w patch set.

### Known still-trapping

None. As of `d53620f` (above), an alpine+node boot + node script execution produces zero illegal-instruction traps. If future workloads surface new ones, the working-tree-only diagnostic at the `illegal_insn:` label will log them.

### Perf profile of `node -e 'console.log(1+1)'` (2026-05-24, wasmtime CLI, native build)

Working-tree counters added in [tinyemu-c2w/riscv_cpu.c](../tinyemu-c2w/riscv_cpu.c) (trap-cause + ECALL-syscall + per-major-opcode histograms; atexit dump) and a counter bump in the interpreter dispatch loop in [tinyemu-c2w/riscv_cpu_template.h](../tinyemu-c2w/riscv_cpu_template.h). Built without OPFS drive (Dockerfile.local sed disabled) so wasmtime CLI can instantiate; missing `c2w_blk` imports stubbed by `wat2wasm`'d /tmp/c2w_blk_stub.wasm and passed via `wasmtime run --preload c2w_blk=...`.

Two-run delta (baseline = `/bin/true`, hot = `node -e 'console.log(1+1)'`):

| Metric | /bin/true (boot+runc) | Node hello | Node-only delta |
|---|---|---|---|
| Wallclock | 2.6s | 13.1s | ~10.5s |
| Total traps | 7,326 | 16,356 | ~9,030 |

Node-only trap breakdown — **page faults dominate, not CSR/syscall traffic**:

| Trap source | Delta count |
|---|---|
| store page faults (cause=15) | 3,049 |
| timer interrupts (S+M, causes 5+7) | 1,942 |
| ecall-U (syscalls) | 1,686 |
| ecall-S (SBI) | 971 |
| external interrupts (cause=9) | 578 |
| load + instruction page faults | 804 |

**Important correction to prior CLAUDE.md claim**: the "16k csrr time traps per Node run" line was wrong. Actual `clock_gettime` calls = 195. The counteren-bits fix would barely move the needle.

Host-side profile (`perf record -F 999`, 12k samples) — **the interpreter loop is everything**:

| Symbol | % host cycles |
|---|---|
| `riscv_cpu_interp_x64` | **87.45%** |
| `get_phys_addr` (MMU walk) | 0.67% |
| `csr_read` / `csr_write` | 0.51% |
| `riscv64_read_slow` / `target_read_insn_slow` | 0.30% |
| everything else (memcpy, wasmtime runtime, etc.) | <12% combined |

So **trap-reduction work is dead** — even eliminating *every* trap would save at most 2-4% of wallclock. The cost is raw guest CPU execution.

Per-RISC-V-opcode histogram (top — full dump in stderr at exit):
- 1.29 billion guest instructions executed in 13.5s = **~95M ops/sec ≈ 10.5 ns per emulated insn**
- Top uncompressed: OP-IMM-32 (9.3%), LOAD (7.4%), OP (7.0%), OP-IMM (4.4%), OP-32 (4.3%), BRANCH (4.3%), STORE (3.1%), JAL (2.5%)
- Compressed (RV-C) ≈ 30% of total, spread across many `insn & 0x7f` buckets

**Why a per-opcode JS bypass doesn't work for this architecture**: TinyEMU *is* already the fast-path — each opcode is hand-written C compiled to wasm and JIT-ed to x86 by the host. Calling out to JS from wasm per-opcode costs ~50-200 ns at the import boundary, ~10× slower than running the C handler directly. The indirection is the wrong shape. The web-specific opcode bypass story only applies to non-emulator-on-emulator stacks (WebContainers, pyodide).

**Raw-perf ceiling without JIT** (Tier 3, months):
1. Computed-goto dispatch + `-O3` → realistic 10-20% (13.5s → ~11s)
2. PGO + inlining → maybe another 10-15%, big build-pipeline complexity
3. Beyond that, only JIT moves the needle (~5-10× on hot blocks)

**Snapshot-after-init** (separate work item, ~weeks): orthogonal to raw perf — it skips the work instead of speeding it up. ~100× on repeated invocations. Will be added behind an on/off flag so future raw-perf changes can be A/B-measured against this baseline.

**Tier 2 perf order** (current plan):
1. Try `-O3` + computed-goto dispatch (incremental wins, days)
2. Snapshot-after-init prototype (weeks)
3. Survey alternative wasm-targeted RISC-V emulators (half day; could change calculus)
4. JIT (Tier 3, months — only if 1-3 still insufficient for agent workloads)

### Tier 2 step 1 verdict: dead end (2026-05-24)

Both attempted wins in step 1 produced **0% measurable speedup**, and one was proved structurally impossible by the toolchain. Documented here so this isn't re-attempted:

**`-O3`** — 3 runs each, `node -e 'console.log(1+1)'` under wasmtime:
- O2 medians: 12.56s / 11.60s / 12.85s (median 12.56)
- O3 medians: 12.49s / 11.38s / 12.52s (median 12.49)
- Verdict: noise. Reverted to `-O2`. Same as upstream default. Reason `-O3` doesn't help here is that the bottleneck is branch prediction on `switch(opcode)` dispatch, not per-handler codegen quality — and `-O3` doesn't touch dispatch structure.

**Computed-goto dispatch** — *proved impossible via this toolchain* by a 4-handler probe in [/tmp/wasi-probe/probe.c](file:///tmp/wasi-probe/probe.c):
- C source: 4 distinct `goto *jt[i]` sites, each at end of a handler (classic threaded-dispatch pattern)
- Compile: `wasi-sdk-19/bin/clang -O2 --target=wasm32` → inspect with `wasm2wat | grep -c br_table`
- Result: **1 br_table** in the generated wasm. All 4 source-level sites collapsed.
- Why: WebAssembly requires **structured, reducible control flow** — no arbitrary `goto`. LLVM's wasm backend (CFGStackify pass) is *required* to merge irreducible CF (which threaded dispatch is) into a single dispatch loop with one `br_table` wrapped in nested blocks. The CPU sees one indirect branch site, not N.
- Verdict: the win we'd get on native x86 with computed goto **literally cannot be expressed in wasm**. The optimization is destroyed at the compile target, not by clang's choices. Refactoring TinyEMU's 950-line dispatch loop to computed goto would produce 0% speedup through this pipeline.
- Cranelift wouldn't fix this either — it lowers one `br_table` to one indirect jump on x86. No incentive to duplicate dispatch sites.

**Net consequence**: the **interpreter-dispatch branch-prediction problem is unfixable without leaving the interpreter-in-wasm model**. The path forward is either:
- Step 2 (snapshot) — skip dispatch entirely on repeat invocations
- Step 4 (JIT, Tier 3) — emit direct branches per guest basic block, bypassing the indirect-branch dispatch altogether

**Revised Tier 2 perf order** (after step 1 exhausted):
1. ~~`-O3` + computed-goto~~ ← exhausted; both 0% above
2. ~~Snapshot-first~~ ← deferred, see below
3. ~~Alt-emulator survey~~ ← user committed to TinyEMU (Bellard's design is best-in-class for RISC-V)
4. **JIT (Tier 3) → promoted to next.** Rationale below.

### Why JIT promoted over snapshot (2026-05-24)

Decision after discussing snapshot vs JIT vs auto-learning-watcher approaches:

- **Snapshot** = skip work entirely. ~50-100× on covered commands, 0× on uncovered. Cheap (weeks) but narrow.
- **Auto-learning watcher** (user's intuition: detect repeats, memoize whole-program state) = research-grade. CRIU-equivalent inside emulated guest plus fingerprinting plus restore correctness. Months-to-years, open problems.
- **JIT** = do work faster. 5-10× on *everything*. Months but generally applicable.

For the actual product target — **agents running hundreds of varied shell-outs per session** (`git status`, `rg`, `cat`, `python`, `node`, custom scripts) — snapshot only covers a handful (the pre-warmed ones). JIT helps all of them, including the kernel paths handling fork/exec/syscalls between them.

The same logic applies to long-term generality: snapshot is per-program one-off engineering; JIT scales with the workload distribution automatically.

**Snapshot is deferred, not abandoned** — it remains the right answer for "first command after page load" cold-start, and may be added later as an orthogonal optimization on top of JIT.

### Tier 3: BB JIT plan (2026-05-24, committed)

#### Architecture (committed decisions)

- **Compilation model**: emit wasm bytecode at runtime, hand to host (`new WebAssembly.Module(bytes)`), call compiled funcs via `call_ref` from a `WebAssembly.Table` of funcref. Cannot emit native machine code from inside wasm — only the runtime can. This is the constraint that shapes everything.
- **Granularity (now)**: per basic block. Block ends at any branch / jump / ecall / fence / trap-instruction.
- **Granularity (later)**: traces (linked BBs with guards). BB infrastructure carries over ~100% to traces.
- **Browser-first target, wasmtime-first dev.** Port to browser worker by week 2-3, not at end. Dual-bench continuously to catch Liftoff-tiering or browser-engine surprises early.
- **Single worker** for emulator + JS coordinator. Sync compile via `new WebAssembly.Module(bytes)` (works off-main-thread). No cross-worker function-ref juggling.
- **Speed > memory**. Aggressive register lifting (all live regs → wasm locals), generous code cache (no early eviction), 8-16 entry inline TLB in each compiled block.
- **One wasm module per compiled BB initially**, batch later if compile-storm latency shows up.
- **No block chaining initially**. Add via funcref-table indirection later for tight-loop wins.
- **MMU staleness via generation counter** (simplest correct invalidation).
- **Required wasm features** (all shipped in Chrome 119+/FF 120+/Safari 17.4+/wasmtime): function references + `call_ref`, tail calls (`return_call`), SharedArrayBuffer (already in use).

#### Code organization

- JIT lives in TinyEMU's [tinyemu-c2w/](../tinyemu-c2w/) tree as new files (`jit_codegen.c`, `jit_dispatch.c`, etc.). The interpreter loop in [riscv_cpu_template.h](../tinyemu-c2w/riscv_cpu_template.h) gets a small fast-path: check block_table → call_ref or fall through to interpret.
- JS coordinator added to the browser scaffold at [/tmp/out-browser/htdocs/](file:///tmp/out-browser/htdocs/). For wasmtime-only testing, a Rust wrapper that hosts the same coordinator role.

#### Batch plan — small enough to A/B-measure each

Each batch has a **concrete success bar** (must hit to continue) and a **bail criterion** (must rethink if hit). Estimates assume focused work; calendar time longer with interruptions.

Baseline to beat at each step: **12.5s** for `node -e 'console.log(1+1)'` (wasmtime CLI, -O2, no JIT). Measurements compared with 3-run median.

**Batch 1 — Infrastructure spike (~3 days)**
- Pick simplest possible "guest hot loop" (handcrafted: `addi x5, x5, 1; jal x0, -4`)
- Hardcode: when interpreter sees that PC, call out to JS coordinator
- JS hand-writes the wasm module for that one block, instantiates, registers in block_table
- Interpreter does `call_ref` to it instead of running the loop
- ✅ **Success bar**: compiled BB runs at all. End-to-end pipeline (dispatch → call_ref → return → next dispatch) works.
- ❌ **Bail criterion**: per-BB dispatch overhead (call_ref round-trip) > 50 ns. That would mean our overhead per BB swamps any per-insn savings, architecture rethink needed.

**Batch 2 — ALU-only codegen (~3 days)**
- Real codegen for: LUI, AUIPC, ADDI, ADDIW, ADD, ADDW, SUB, SUBW, AND, OR, XOR, SLLI, SRLI, SRAI, SLT, SLTU
- Block discovery walks forward from PC until non-supported opcode
- Block must end at branch/jump (next batch); for now only compile blocks whose tail is the special "guest hot loop" branch
- ✅ **Success bar**: 2× speedup on a synthetic ALU-heavy guest loop (e.g., 1M iter of arithmetic), measured vs same loop interpreted.
- ❌ **Bail criterion**: <1.2× speedup. Means cranelift isn't lowering our wasm well; need to inspect emitted x86.

**Batch 3 — Branches + jumps (~3 days)**
- Add codegen for: BEQ, BNE, BLT, BGE, BLTU, BGEU, JAL, JALR
- Block discovery completes (proper block-end detection)
- Block-exit returns next PC, main loop dispatches
- ✅ **Success bar**: 1.5× speedup on `busybox echo hi` (small real guest workload), block coverage >50% of executed insns.
- ❌ **Bail criterion**: <1.2× speedup OR block coverage <30%. Diagnose.

**Batch 4 — Loads/stores via slow-path call (~4 days)**
- Add codegen for: LB, LH, LW, LD, LBU, LHU, LWU, SB, SH, SW, SD
- Each memory access calls into existing `target_read_*`/`target_write_*` helpers as imports
- Slow (full MMU walk per access) but unlocks much more block coverage
- ✅ **Success bar**: block coverage >80% of executed insns on `busybox echo hi`. End-to-end speedup ≥ 1.5× still holds.
- ❌ **Bail criterion**: speedup regresses below batch 3 (slow-path call overhead eats gains). Means inline TLB must come before coverage expansion.

**Batch 5 — Inline TLB (~3 days)**
- 8-16 entry TLB in shared memory; compiled blocks probe inline
- TLB miss → slow-path call (rare)
- ✅ **Success bar**: 2× speedup vs batch 4 on the same workload. End-to-end ≥ 3× vs interpreter baseline.
- ❌ **Bail criterion**: <1.3× speedup vs batch 4. TLB miss rate likely too high — profile what's missing.

**Batch 6 — Register lifting w/ liveness (~2 days)**
- Per-block liveness analysis (cheap: linear scan)
- Lift only live regs at entry, spill only modified at exit
- ✅ **Success bar**: 1.2-1.5× speedup vs batch 5 on Node hello.
- ❌ **Bail criterion**: <1.1× speedup. Probably cranelift already lifts well from the naive load/store pattern; not worth keeping the complexity.

**Batch 7 — RV-C (compressed) insns (~3 days)**
- ~30% of executed insns are compressed; without these, lots of blocks die early
- Add codegen for all hot quadrant-0/1/2 patterns
- ✅ **Success bar**: block coverage >95% on Node hello. End-to-end speedup ≥ 4× vs interpreter baseline.
- ❌ **Bail criterion**: coverage doesn't move much (means we were already hitting the right blocks).

**Batch 8 — Hot CSRs inline (~2 days)**
- `rdtime`, `rdcycle`, `csrr time/cycle` inline instead of slow-path
- Skip rare CSRs (rdinstret, custom CSRs) — let them slow-path
- ✅ **Success bar**: trap rate per Node run drops measurably; small additional speedup.

**Batches 9+ — TBD from profile**
- After 8, re-profile (`perf record -F 999` on wasmtime, or `--profile=perfmap`)
- Likely candidates: block chaining (tight loops), batched module compile (reduce per-block overhead), trace recording (start of Tier 3 phase 2)

#### Browser port plan (interleaved, not deferred)

- After **Batch 3**: port basic dispatch to browser worker, confirm `call_ref` + `WebAssembly.Module(bytes)` sync compile work as expected in Chrome + Firefox. Catches engine differences before they're baked in.
- After **Batch 5**: rerun perf comparison in browser. Look for Liftoff-tier surprises (cold blocks slower than expected for first ~N hits before TurboFan kicks in).
- After **Batch 7**: this is the "demo-ready" moment. Real speedup visible end-to-end in browser. Time to update the public demo page.

#### Open architectural questions (revisit at the relevant batch)

- **Q1 — batch size for compile**: start at 1 BB / module. Revisit after Batch 4 if compile-burst latency is visible.
- **Q2 — block chaining**: defer until after Batch 7. Need real workload to know which BB→BB transitions are hot enough to justify chaining infrastructure.
- **Q3 — MMU/code invalidation**: bump a generation counter on SATP write, FENCE.I, or known-bad MMU events. Each compiled block checks generation at entry; mismatch → return to interpreter. Revisit if check overhead is measurable.

#### What stays deferred (snapshot work)

Boot snapshot + warm-pool (`node`/`python`/`bash` pre-warmed) remains a valuable orthogonal optimization on top of JIT. Will be revisited after JIT plan reaches a stable end-to-end speedup, likely as a Tier 4 / "first-command latency" workstream.

#### Batch 1 result (2026-05-24): PASS — dispatch overhead ≈ 0

End-to-end pipeline works (Node hello runs correctly through `wasmtime --preload jit=jit_helpers.wasm ... alpine-node-jit2.wasm node -e ...`). Per-BB dispatch overhead measurement on `node -e 'console.log(1+1)'` (5 runs each, median, ~150M block-entries per run):

| Variant | Median wall | Δ vs V0 | per-BB cost |
|---|---|---|---|
| V0 — no JIT hook (interpreter only) | 11.43s | — | — |
| V1 — hook calls `jit.dispatch_noop` (returns 0) | 11.47s | +40ms | ~0.3 ns |
| V2 — hook calls `jit.dispatch_indirect` → `call_indirect` to noop block | 11.63s | +200ms | ~1.4 ns |

Bail criterion was **50 ns/BB**; measured ~1.4 ns/BB. Cleared by ~35×. Even pessimistically bounding the differential by full 5-run spread (~1s) gives <7 ns/BB. Wasmtime/Cranelift compiles cross-module `call_indirect` to essentially a regular x86 indirect call — no thunk overhead visible at this resolution.

**Architectural consequence**: the per-BB dispatch design is sound. Future batches can stay BB-granular without worrying about dispatch-side amortization. Block chaining (Q2 above) remains deferred.

**Code added** (all working-tree only, not yet committed):
- [tinyemu-c2w/jit_interface.h](../tinyemu-c2w/jit_interface.h) — variant-selectable JIT import declaration (`JIT_DISPATCH_VARIANT={0,1,2}` build flag)
- [tinyemu-c2w/jit_helpers.wat](../tinyemu-c2w/jit_helpers.wat) + `.wasm` — hand-written dispatch helper module (block_table + dispatch_noop + dispatch_indirect)
- [tinyemu-c2w/riscv_cpu_template.h](../tinyemu-c2w/riscv_cpu_template.h) — block-start hook + `bb_entries` counter
- [tinyemu-c2w/riscv_cpu.c](../tinyemu-c2w/riscv_cpu.c) — `bb_entries`/`jit_hits` counters + dump
- [Dockerfile.local](Dockerfile.local) — `ARG JIT_DISPATCH_VARIANT=0` plumbed into CC flags

**Repro command**:
```bash
./out/c2w --target-arch=riscv64 --dockerfile ./Dockerfile.local \
  --extra-flag --build-context=tinyemu-local=/home/and/Projects/tinyemu-c2w \
  --build-arg OPTIMIZATION_MODE=native --build-arg JIT_DISPATCH_VARIANT=2 \
  alpine-node-rv64:local ./out/alpine-node-jit2.wasm

wasmtime run \
  --preload jit=/home/and/Projects/tinyemu-c2w/jit_helpers.wasm \
  --preload c2w_blk=/tmp/c2w_blk_stub.wasm \
  out/alpine-node-jit2.wasm --no-stdin node -e 'console.log(1+1)'
```

#### Batch 2 prep

Batch 2 ("ALU-only codegen, 2× on synthetic loop") requires **dynamic** wasm module construction at runtime — `--preload` of static modules won't suffice once we want PC-keyed runtime-compiled blocks. Need a host embedder. Per plan: Rust + wasmtime crate. Install rustup before starting Batch 2 work. Tooling decision: write the embedder as a thin host that loads the c2w wasm, provides the `jit.*` imports, and exposes a `register_block(pc, wasm_bytes, len)` host fn callable from inside c2w (so the interpreter can submit codegen results back to the host's block table).

#### Batch 2 result (2026-05-25): 1.25× on Node arithmetic loop — between bail (<1.2×) and success (≥2×)

End-to-end pipeline works. Pure ALU coverage compiles + executes correctly across the full Node startup and a tight `for(let i=0;i<1e8;i++) s+=i*7&255` loop.

Wallclock medians, 3 runs each (Ryzen 9 7000, wasmtime via [jit-host](jit-host/) embedder):

| Variant | What it does | Median | Δ vs jit2 |
|---|---|---|---|
| jit2 — Batch 1 dispatch_indirect, no scanner | every BB → dispatch miss → fall through to interpreter | 114 s | — |
| jit3 — Batch 2 (AUIPC disabled, see below) | first-miss scan, ALU runs compiled, dispatch hits = 285M (16.5%) | 91 s | **1.25×** |

Coverage ceiling for ALU-only is small: only ~8% of executed insns end up inside a compiled block (each block averages ~3 ops before the scanner hits a non-ALU and stops). Runs of pure ALU between branches in V8-emitted RV64 are 2-4 insns; this caps Batch 2's reach. To clear the 2× success bar will need Batch 3 (branches+jumps; should jump coverage well above 50%).

**AUIPC stays disabled.** Enabling it correctly compiles + executes — under wasmtime cranelift — until somewhere ~300 compiled blocks in, when Node corrupts and exits silently. Bisected to the block at pc=0x4d984; that block in isolation is also fine. Strongest theory: code-invalidation we don't yet handle (FENCE.I / SATP-switch with a stale baked PC value in a compiled block). PIC-pattern AUIPC blocks are particularly exposed because their value is PC-dependent — non-AUIPC ops produce PC-independent results that survive most invalidation classes. Plan Q3 ("MMU/code invalidation: bump a generation counter on SATP write, FENCE.I, ... compiled blocks check generation at entry") is the right fix; postponed to before Batch 4 lands (loads/stores would magnify the same exposure).

**Bug fixes uncovered during Batch 2 (committed)**:
- riscv_cpu_template.h JIT hook now resets `code_ptr = NULL; code_end = NULL;` after a compiled-block return, so the interpreter re-walks the TLB instead of mis-decoding from the previous block's page bytes. Without this, only the very first compiled block worked.
- jit-host injects `--no-stdin` as guest arg 0. wasmtime CLI repro had this; without it, c2w's `poll_oneoff` on a non-TTY host stdin returns EINVAL and the kernel exits before runc launches the container command. This was hidden in earlier Batch 1 tests because no node script was actually being executed (no failure → no signal).
- dispatch_indirect avoids `func.typed::<i32, i64>()` on every call (caches `TypedFunc` at register_block time). Per-dispatch overhead from ~50ns → ~5ns; the timing was masking the speedup signal.

**What committed where**:
- tinyemu-c2w `97c8f0c` — jit_codegen.c (scanner+IR), scanner hook in template, jit_interface.h adds register_block/mark_uncompilable, Makefile picks up jit_codegen.o.
- container2wasm `7e6fc69` — jit-host codegen.rs/ir.rs, register_block + TypedFunc-cached dispatch_indirect, Dockerfile.local adds jit_codegen.o to EMU_OBJS.

**Next step decision**: at 1.25× we're past bail but short of success. Two paths:
1. Push to Batch 3 (branches+jumps). Likely takes us well past 2× because branch-bound BBs become single compiled blocks instead of 5-10 dispatch-miss interpreter trips.
2. Implement Q3 invalidation first, re-enable AUIPC. Smaller incremental win (probably to ~1.4-1.5×) and lays the foundation for Batch 4 (loads/stores) where invalidation hygiene matters more.
Default plan: do (1) next, fold (2) in before Batch 4.

#### Batch 3 result (2026-06-10): correctness landed (incl. Q3 invalidation + AUIPC back on); perf regressed — dispatch architecture is now the bottleneck

**The hang and its root cause.** With branch codegen enabled, Node hello hung deterministically after ~556 branch blocks compiled — guest spinning ~23M block-entries/sec in a fixed 6-PC kernel wait loop (trace: `JIT_TRACE_DISPATCH=N` env, dispatch ring buffer dumped at exit; epoch-based `JIT_TIMEOUT_SECS=N` turns hangs into clean traps). Per-block codegen was verified correct three ways: standalone semantic test of the bisected block ([jit-host/tests/block_n555.rs](jit-host/tests/block_n555.rs)), bit-level desk-check of the scanner's B/J-type immediate decode, and desk-check of all six branch condition mappings. The actual bug: **blocks are keyed by virtual PC with no invalidation**, so compiled blocks survived address-space switches (satp writes), PTE updates (sfence.vma), and V8's self-modifying code (fence.i) — stale code ran at reused VAs and corrupted a userspace process; the kernel loop was just the downstream wait-for-event symptom. The Batch 2 "AUIPC corruption" was this same class (a baked PC-relative constant is no more exposed than a baked branch target); AUIPC is re-enabled and Node hello passes.

**Fix shape (committed)**: TinyEMU calls a new `jit.flush_blocks(kind, addr)` import from the three mapping-change points — satp write (kind 0), sfence.vma (kind 1, addr-targeted when rs1!=0), fence.i (kind 2) — and clears its scan tried-cache. Host policy: satp drops only user-half blocks (kernel half is globally mapped), targeted sfence drops one page (blocks never span pages), fence.i drops all. Naive drop-and-recompile starved the run (wasmtime's default 10k-instances-per-Store cap + 107s of cranelift churn), so the host keeps a **content-addressed cache** keyed by (end_pc, IR bytes): a flush only drops pc→block mappings; re-registration after rescan re-links the existing instance. Staleness-immune by construction (scanner reads current bytes → changed code = different key) and absorbs 95%+ of re-registrations. Store limit raised to 1M instances via StoreLimits (`cache=10-12k` unique blocks on Node hello — V8 SMC mints new content continuously). `JIT_IGNORE_FLUSH=1` A/Bs invalidation off on the same wasm.

**Numbers (Ryzen 9 7000, 3-run medians where noted)**:
| Workload | Interp baseline | Batch 2 (ALU) | Batch 3 (br+jmp+AUIPC+invalidation) |
|---|---|---|---|
| node hello | 12.5s | ~13s | 17.6s (no AUIPC) / 21.7s (AUIPC) |
| 1e8 arith loop | 114s (jit2) | 91s (1.25×) | **161s — regression** |

Output correct in all runs; `reg_fail=0`; hello compiles ~10k unique blocks (~8.5s cranelift at ~0.85ms/module — the "batch many BBs per module" item is now real).

**Why perf regressed — the load-bearing finding**: every block entry crosses wasm→host (`dispatch_indirect` closure: HashMap lookup ~35ns), and hits pay host→wasm re-entry (~150ns) on top. A 3-4-insn block interprets in ~40ns, so under host-side dispatch **small blocks are net-negative** — the 1e8 run made 1.86B entries (474M hits) and the boundary costs swamp the codegen wins. Batch 1 measured 1.4ns/entry precisely because dispatch then lived inside wasm (static preloaded table + `call_indirect`); Batch 2 moved dispatch into a host closure to get dynamic registration, and that expediency is now the dominant cost.

**Next step (before Batch 4)**: move dispatch back into wasm — host boundary crossed only at registration/flush, never per block entry. Note this matters double for the browser target: there the host is JS, and wasm↔JS crossings are slower than wasm↔Rust, so the current shape would regress even harder in-browser. Concrete design:
- Export the c2w module's own indirect-function table (`-Wl,--export-table` in the wasi-sdk link). The host registers a compiled block by `table.grow`/`table.set`-ing the block's `Func` into that table (wasmtime allows inserting any same-Store Func, including ones from per-block modules) and handing the slot index back from `register_block`.
- C-side dispatch becomes a plain function-pointer call: `typedef uint64_t (*bb_fn)(void *state); next_pc = ((bb_fn)idx)(s);` — in LLVM-wasm a C function-pointer call IS `call_indirect` on table 0, and the cast's signature (i32)->i64 matches the block type exactly, so the type check passes. No builtins, no reference-types flags.
- C keeps the pc→idx map in linear memory (direct-mapped or small open-addressed hash, like the tried-cache). Miss = map lookup fails → interpret/scan. Flush = clear map entries (host keeps a slot free-list so the table doesn't grow unboundedly under SMC churn).
- Target: restore ~1.4ns-class dispatch (Batch 1's measured number for in-wasm `call_indirect`) so Batch 3's coverage translates into wallclock wins; re-measure 1e8 (expect well under 91s) and busybox echo hi (the formal ≥1.5× gate) before starting loads/stores.

**What committed where**: tinyemu-c2w `bafb2e4` (invalidation hooks, tried-cache global, AUIPC re-enable, n_cycles charge per compiled block), container2wasm — jit-host flush_blocks + content cache + StoreLimits + diagnostics (`JIT_TIMEOUT_SECS`, `JIT_TRACE_DISPATCH`, `JIT_DUMP_PCS`, `JIT_NO_BRANCH`/`JIT_NO_JUMP`, bisect-bad-block.py), lib.rs split + tests/.

#### Batch 3.5 result (2026-06-10): wasm-side dispatch + scan-gate — JIT decisively net-positive for the first time

Implemented exactly the committed design (variant 4, `JIT_DISPATCH_VARIANT=4`), plus one addition the first measurement round forced: a **hotness threshold**. Numbers (3-run medians, Ryzen 9 7000, jit-host):

| Workload | interp | Batch 2 (ALU) | Batch 3 (host dispatch) | v4 dispatch | **v4 + scan-gate** |
|---|---|---|---|---|---|
| 1e8 arith loop | 114s | 91s | 161s | 98.6s | **75.5s (1.51× vs interp)** |
| node hello | 12.5s | ~13s | 21.7s | 20.0s | **14.8s** |
| busybox echo hi | 1.57s | — | — | 5.5s | **3.04s** |

All outputs correct, `reg_fail=0`, and `dispatch_hit/miss=0` on the host — the boundary is never crossed per block entry. Adversarial review workflow (10 agents, 4 lenses): 0 confirmed findings.

**What was built** (tinyemu-c2w `f6f016e`, container2wasm this commit):
- **Wasm-side dispatch**: c2w links with `-Wl,--export-table -Wl,--growable-table`; host `table.grow`s each compiled block's `Func` into the c2w module's own table 0 and `register_block` returns the slot index. C keeps a direct-mapped 64K-entry pc→slot map in linear memory; a hit is a plain function-pointer call (`((c2w_bb_fn)(uintptr_t)idx)(s)` = `call_indirect`, cross-module type check passes via wasmtime's engine-wide type canonicalization). Works exactly as Batch 1 predicted.
- **O(1) lazy invalidation** replacing host-side eager flush: three generation counters (user / global / per-page-hash, 4096 buckets) stamped into each map entry at insert and checked at lookup. satp → user_gen++ (kernel half survives, relies on Linux announcing kernel-text changes via fence.i); targeted sfence.vma → page_gen[hash]++; fence.i / full sfence → global_gen++. No memsets on flush anymore. flush_blocks host import is stats-only now.
- **Scan-gate (the forced addition)**: first v4 measurement showed echo hi at 5.5s vs 1.57s interp — **3.0s of it cranelift-compiling 5145 blocks of once-through boot code** (hello: 8.9s compiling 10.3k blocks). A JIT cannot win on code that runs once; it must not compile it. Gate: a pc is scanned only after 16 dispatch misses within the current mapping epoch (gen-stamped like the map, so flushes reset counts lazily; previously-hot pcs re-scan immediately after an epoch roll — the host content cache makes that ~µs; failed scans set an uncompilable sentinel until the epoch rolls). Override with `-DJIT_HOT_THRESHOLD=N`. Effect: hello unique compiles 10,292 → 2,120, compile_ms 8.9s → 1.7s; echo hi compile_ms 3.0s → 0.83s.

**Where the remaining gaps are** (why hello is 14.8 not <12.5, echo hi 3.0 not 1.6):
- ~1.7s/0.8s residual cranelift time — next lever is batching many BBs per module (~0.85ms/module overhead) and/or raising the threshold.
- ~0.5-0.6s miss-path overhead on once-through code (map + gate lookup per block entry on code that never compiles). Even at zero compile cost echo hi would sit ~2.2s vs 1.57s — **boot-heavy workloads can't beat the interpreter until coverage rises**, i.e. until the kernel's actually-hot paths (memcpy, page ops, scheduler) become compilable. That's Batch 4 (loads/stores) + Batch 7 (RV-C).
- The formal "≥1.5× on busybox echo hi" Batch 3 gate is therefore **not met and not meetable at current coverage** — the 1.51× gate is met on the hot-loop workload instead. Carrying the gate forward to re-test after Batch 4.

**Decisions locked by this batch**: dispatch architecture is final (in-wasm, host only at registration/flush — also the right shape for the browser port where the host is JS). Invalidation is final (gen counters, content-cache reuse). Q1 (compile-burst latency) answered: hotness gate, not module batching, was the first-order fix; batching remains a second-order lever.

**Next**: Batch 4 (loads/stores via slow-path calls into `target_read_*`/`target_write_*`), re-measure echo hi gate after; then inline TLB (Batch 5), RV-C (Batch 7) — order per original plan.

#### Batch 4 result (2026-06-10): loads/stores land — first config to beat Batch 3.5 on every workload; scan-gate default rebalanced to 128

**Design as built** (tinyemu-c2w `9a6b734`, container2wasm this commit):
- Scanner emits 11 new IR kinds (LB/LH/LW/LD/LBU/LHU/LWU/SB/SH/SW/SD, kinds 38-48); loads/stores are non-terminators, so blocks now run through memory traffic instead of dying at the first load. Load-to-x0 still performs the access (rd_off=0 sentinel skips writeback); stores excluded from the rd==0 short-circuit (their rd field is imm bits).
- c2w exports `c2w_jit_lb`..`c2w_jit_sd` (full insn semantics: inline TLB-probed `target_read_u*`/`target_write_u*` fast path, sign/zero extension, rd writeback). The host resolves them into each block module's **function imports at instantiation**, so a guest load/store is a wasm→wasm call — the host boundary is never crossed, same principle as v4 dispatch. Blocks declare only the helpers they use (pure-ALU blocks keep the old shape; old artifacts still work under the new jit-host).
- **MMU-fault convention**: helper returns 1 (pending_exception/tval already set by the slow path); the block bails returning `fault_pc | 1` (real pcs are even ⇒ bit-0 tag is unambiguous). The dispatch hook unmasks, sets `s->pc = fault_pc`, resyncs `code_ptr/code_to_pc_addend` (so GET_PC() stays right even if the *fetch* then faults), and falls through — the interpreter re-executes the faulting insn and raises with correct epc/tval. No new exception plumbing; re-execution is architecturally equivalent to RISC-V fault-restart semantics. Per-insn fault pcs ride in the unused IR field (loads: rs2_off, stores: rd_off, as byte offset from block start); since codegen now bakes `start_pc + off`, the content-cache key gained start_pc.
- Semantic tests: [jit-host/tests/mem_ops.rs](jit-host/tests/mem_ops.rs) (arg derivation, import wiring, fault tag + skip-rest-of-block).

**The forced rebalance**: mem coverage made ~3× more pcs compilable, and at gate=16 cranelift swamped short workloads (echo hi 5.18s — worse than Batch 3.5's 3.04; hello compile_ms 1.7→5.25s). Threshold A/B (16/64/128, 3-run medians) showed 128 dominating on all three workloads, so 128 is the new default (`jit_interface.h` + Dockerfile ARG):

| Workload | interp | Batch 3.5 (g16) | B4 g16 | B4 g64 | **B4 g128** |
|---|---|---|---|---|---|
| 1e8 arith loop | 114s | 75.5s | 71.8s | 68.4s | **67.2s (1.70×)** |
| node hello | 12.5s | 14.8s | 15.4s | 13.87s | **13.38s** |
| busybox echo hi | 1.57s | 3.04s | 5.18s | 3.57s | **2.82s** |

All outputs correct, `reg_fail=0`. Bail criterion (slow-path call overhead regressing perf below Batch 3) **not hit** — execution-side time improved everywhere; the only regression channel was compile volume, fixed by the gate.

**Coverage reality check** (working-tree op-counter diagnostics, insns interpreted vs total): on 1e8, ~30% of executed insns now run inside compiled blocks (43% of block entries are dispatch hits). On echo hi only ~5% — **by design**: the gate refuses once-through boot code, and echo is nearly all boot. The original Batch 4 bar ("coverage >80% on echo hi") predates the scan-gate and is intentionally unmeetable now; the meaningful gates going forward are hot-path coverage + wallclock. The carried "≥1.5× on echo hi" gate stays unmet (0.56×) — echo is bounded by ~0.85s of residual compile + miss-path overhead on never-compiled boot code.

**What blocks coverage now**: RV-C. ~30% of executed insns are compressed and every one terminates a scan, so average compiled-block length stays ~3-4 insns and the interpreted 70% on 1e8 is heavily compressed-insn-bound. Also still terminating: MUL/DIV, atomics (LR/SC/AMO), FP, CSR ops.

**Next (proposed reorder)**: RV-C (Batch 7) before inline TLB (Batch 5) — coverage is the bigger lever than per-access cost on current data, and longer blocks also raise the value of register lifting (Batch 6). Residual cranelift cost levers if short-workload latency matters sooner: BB-batching per module, async/background compile off the guest's critical path.

#### Batch 7 result (2026-06-11): RV-C lands + two systemic fixes — and a baseline correction that reframes the scoreboard

**What was built** (tinyemu-c2w this commit, container2wasm this commit):
- **RV-C codegen**: `decode_one_c` in jit_codegen.c expands every RV64C integer insn to the base-ISA IR kind it abbreviates (c.addi4spn→ADDI, c.j→JAL, c.jalr→JALR, c.lw/c.ld/c.sw/c.sd & SP-forms→Batch 4 mem kinds, misc-ALU→SUB/XOR/OR/AND/SUBW/ADDW, etc.); the scan loop reads 2 bytes, dispatches on `(lo & 3) == 3`, and advances by true insn length. FP/reserved encodings still terminate. **Zero host codegen changes**: the IR's absolute-target design (end_pc = link/fallthrough, baked at scan time) made compressed terminators (c.jalr links pc+2, c.beqz falls through pc+2) correct for free. Decode mirrors riscv_cpu_template.h's C_QUADRANT cases bit-for-bit — including TinyEMU's spec deviations (c.lui imm==0 executes; c.addiw rd==0 is a nop; c.lwsp/c.ldsp rd==0 perform the access, discard the value) — so compiled blocks stay bit-identical to interpretation. Hash functions were already `pc >> 1`; 2-byte-aligned block starts just work.
- **Exact cycle charge (time-dilation fix)**: the hook charged 1 n_cycle per block entry regardless of length. Guest virtual time is insn_counter-derived (utime CSR, mtime), so that undercharge dilates guest time proportional to insns-per-block — once RV-C doubled block length, the guest measurably spun extra (bb_entries +10% on the 1e8 loop). jit_map_entry now carries n_insns; the hook charges it exactly. jit_map_lookup returns the entry pointer (same cache line) instead of fn_idx.
- **Guest-side re-link cache (registration-storm fix)**: RV-C tripled compilable pcs, and every epoch roll (12-24k fence.i/sfence.vma per Node run) made every hot pc re-register through the host (Batch 4: 64k registrations/run; RV-C at gate 128: 567k). Since lazy invalidation means the host never drops table slots, a rescan that reproduces identical content can re-link locally: jit_map_entry stores a 64-bit FNV of (pc, end_pc, IR bytes); match ⇒ refresh gen stamps, skip the host call. Registrations on the 1e8 run: 567k → 2.9k (relinks=202k stay guest-side). Saved ~5s on the loop at gate 128; matters double for the browser port (host = JS).
- **Scan-gate default 128 → 512** (jit_interface.h + Dockerfile.local ARG): RV-C re-ran the Batch 4 story — more coverage ⇒ more compilable pcs ⇒ cranelift swamps short workloads. A/B at 128/256/512 (single runs, same box, relink in): 512 dominates hello (13.5 vs 16.7/15.8) and loop (90.2 vs 96.9/92.7), ties 256 on echo (4.2 vs 3.9). The marginal blocks below 512 hits contribute ~2% of executed insns and 2-3× the cranelift time.

**Numbers (3-run medians, same day, same box — older CLAUDE.md numbers are NOT comparable, see correction below)**:

| Workload | jit0 (true interp) | Batch 4 artifact | **Batch 7 (gate 512)** |
|---|---|---|---|
| 1e8 arith loop | **78.7s** | 94.5s | 89.4s |
| node hello | **11.2s** | 14.8s | 13.5s |
| busybox echo hi | **2.4s** | 4.1s | 4.2s |

All outputs correct, reg_fail=0. Batch 7 beats Batch 4 same-day on hello (−1.3s) and loop (−5.1s), ties echo. Coverage (insns in compiled blocks, vs jit0 totals): loop 30%→**46%**, hello →**57%**, echo →23%; avg insns/block-entry 4.3–6.4 (was ~3-4).

**The baseline correction — load-bearing, read this**: the "interp 114s" used as the loop baseline since Batch 2 was actually the **jit2 artifact** — Batch 1's dispatch_indirect build paying a wasm→host crossing per block entry — not a clean interpreter. The true interpreter (jit0, no JIT hook at all) runs the loop in **78.7s** (7.0ns/insn), hello in 11.2s, echo in 2.4s (today's box; the Ryzen runs 10-40% slower day-to-day under WSL2, so only same-day ratios are meaningful — which is also why Batch 4's "67.2s" and today's 94.5s for the same artifact don't contradict). Against the true baseline, **the JIT has never beaten the interpreter on any of the three workloads** — Batch 3.5/4's "1.5-1.7× vs interp" claims were really "vs a crippled JIT build". The honest current state: JIT = 0.88× interp on the hot loop, 0.83× on hello, 0.57× on echo.

**Why compiled code only reaches parity per-insn (loop decomposition)**: 89.4s − 3.6s compile = 85.8s execution. Interpreted insns at 7.0ns cost 42.4s, leaving ~43s for 5.11B compiled insns + 2.1B dispatch decisions ⇒ ~7-8.5ns per compiled insn — no better than interpretation. The interpreter's own per-insn path is already lean (fetch from TLB-cached page pointer, direct switch); the compiled block pays a call_indirect entry, per-op loads/stores of guest regs to linear memory, and — dominant — **mem ops as wasm→wasm helper calls (~20ns each: call + C inline-TLB probe + sign-extend + writeback) while ~30% of executed insns are loads/stores** (V8 baseline-JIT code is stack-machine dense). 0.3×20ns + 0.7×~2ns ≈ 7ns. The codegen wins on ALU are real but fully consumed by mem-helper calls and entry overhead at 4-6-insn block granularity.

**Consequences for the plan**:
1. **Batch 5 (inline TLB inside compiled blocks) is now clearly the highest-leverage step** — it attacks the dominant per-insn cost directly. Back-of-envelope: mem ops 20ns→4ns takes the loop's compiled mix to ~3.5ns/insn ⇒ loop ≈ 70s < 78.7s interp. That flips the first sign.
2. **Block chaining / longer regions** (deferred Q2) rises to second: entry overhead amortizes only with length, and 46-57% coverage at 4-6 insns/block means dispatch is still ~2.1B decisions per loop run. Register lifting (Batch 6) pays off only after blocks get longer.
3. **Re-baseline everything against jit0 (true interp) from now on**; same-day medians only. The carried "≥1.5× echo" gate is retired as misformulated — the meaningful gates are per-workload wallclock vs jit0.
4. Echo stays interp-favored until compile cost leaves the critical path (async compile remains the lever; 1.4s of echo's 4.2s is cranelift).

**Next**: Batch 5 (inline TLB), then chaining, then re-evaluate Batch 6/8. MUL/DIV + atomics (LR/SC/AMO) are the next coverage terminators worth folding in along the way (V8 emits both heavily).

#### Batch 5 result (2026-06-11): inline TLB lands correct — and measures as a net LOSS; shelved to opt-in. Two cost-model corrections come out of it

**What was built** (tinyemu-c2w this commit, container2wasm this commit): compiled blocks probe TinyEMU's live TLB inline — same tag compare as the C macros (`vaddr == addr & ~(PG_MASK & ~(bytes-1))`, alignment folded in), hit ⇒ direct load/store on the imported linear memory at `mem_addend + wrap32(addr)`, miss ⇒ the Batch 4 helper call (which refills via the slow path). Correct by construction: it reads the same live entries the interpreter uses (tlb_flush writes vaddr=-1 into those very words; entries are only ever installed for RAM pages, so MMIO can't hit the fast path; write-entry install happens on a slow-path store that already set the dirty bit). Zero IR changes — re-link cache and content-cache keys unaffected. Plumbing: new guest export `c2w_jit_tlb_layout(selector)` (offsets of tlb_read/tlb_write, TLB_SIZE, TLBEntry size/field offsets, PG_SHIFT — selector-keyed so additions can't break older hosts); host queries it once at startup and bakes shifts/masks into block codegen. 4 new semantic tests (hit-skips-helper incl. addend arithmetic, miss-falls-back, misaligned-forces-miss, miss-fault-bails). All 3 workloads produce correct output, reg_fail=0.

**Numbers (same-session, 3-run medians + decisive single-run A/B on the same jit5 artifact)**:

| Config | echo | hello | loop |
|---|---|---|---|
| jit0 (true interp) | 1.7s | 11.4s | 68.5s |
| jit7 (Batch 7 artifact) | 2.9s | 11.7s | 68.6s |
| jit5, inline TLB ON (now opt-in `JIT_INLINE_TLB=1`) | 3.0s | 12.8s | 70.9s |
| jit5, helper calls (now the default) | — | — | **68.5s** |
| jit5, `JIT_NO_MEM` (no mem codegen) | — | — | 72.4s |

The A/B pins it: same artifact, the probe codegen alone costs +2.0s execution +0.4s compile on the loop (and +1.1s on hello). The miss-rate explanation is excluded by the NOMEM row — if inline probes missed, the helpers' bit-identical C probe would miss too and mem ops would go through the page walker at 50-100ns each, tens of seconds slower; instead helper-shape mem codegen is worth ~4s net. Probes hit; the win just isn't there.

**Correction 1 — the mem-op cost model was wrong.** Batch 7's decomposition attributed ~20ns to each wasm→wasm helper call; the real cross-module call cost under wasmtime is a few ns. Eliminating it cannot pay for ~30 extra wasm ops of probe per mem op across ~3.2k blocks (icache + regalloc + ~15% more cranelift time) versus one shared clang-optimized helper that stays icache-resident. Batch 7's "7-8.5ns per compiled insn ⇒ mem helpers dominate" arithmetic is retracted; per-op micro-costs are smaller and flatter than modeled. Inline TLB is now opt-in (`JIT_INLINE_TLB=1`), kept tested-and-working for the browser port, where an engine with expensive cross-instance calls would flip the calculus back.

**Correction 2 — the box invalidates ratios *within* a day.** This morning's Batch 7 medians: jit0 78.7s / jit7 89.4s (0.88×). This afternoon, same artifacts, same harness, same script: jit0 68.5s / jit7 68.6s (1.00×). Compile_ms also halved (3.6s → 2.8s). WSL2 box state shifts both absolute speed AND relative ratios between JIT and interpreter builds. Methodology from now on: conclusions only from same-session A/B with all configs interleaved, and effects under ~5-10% are treated as unresolved on this box regardless of medians.

**Honest current state (this session)**: loop and hello at parity with the true interpreter (the morning's "JIT loses everywhere" was partly box state); echo still 1.7× behind on compile cost (2.1s of echo's 3.0s is cranelift).

**Consequences for the plan**: with per-mem-op cost off the table, the remaining gap drivers are (a) compile cost on the critical path — dominant on echo and the first seconds of every workload, (b) block entry/dispatch overhead × 4-6-insn blocks — ~2.1B dispatch decisions per loop run, (c) coverage terminators. Reordered next steps:
1. **Coverage: MUL/DIV + atomics (LR/SC/AMO)** — cheap to add, V8 emits both heavily, and longer blocks amortize entry overhead (attacks (b) and (c) together).
2. **Block chaining / longer regions** (Q2) — direct attack on (b).
3. **Async/background compile** — takes (a) off the guest's critical path; echo's lever.
4. Register lifting (Batch 6) stays parked until blocks are longer; inline TLB re-evaluated at browser-port time.

#### Batch 4.5 result (2026-06-11): RV64M + RV64A coverage lands — strictly dominates Batch 5 (hello −10%, echo −15%, loop tie); coverage's wallclock ceiling now measured

**What was built** (tinyemu-c2w `3e06378`, container2wasm this commit):
- **RV64M inline**: MUL/MULH/MULHSU/MULHU/DIV/DIVU/REM/REMU + W-forms (IR kinds 49-61) compile to wasm arithmetic. Wasm div/rem trap where RISC-V defines results, so codegen guards them (div/0 = −1, rem/0 = dividend, INT_MIN/−1 = INT_MIN with rem 0 — wasm's `rem_s` already yields 0 there without trapping; W-forms pre-narrow operands so the 64-bit wasm ops are exact 32-bit arithmetic). The mulh family synthesizes the true 128-bit high word from four 32×32 partial products plus sign-correction identities (mulh = mulhu(a,b) − ((a≫63)&b) − ((b≫63)&a)). **Trap discovered en route**: TinyEMU's non-int128 `mulh`/`mulhsu` fallback in riscv_cpu_template.h subtracts the *wrong operand* (`r1 -= a` under `a < 0`; correct is `b`) — but it's dead code: the wasm build defines `HAVE_INT128` (`__SIZEOF_INT128__`; confirmed via `__multi3` in the artifact), so the interpreter computes the true high word and codegen matches that. If anyone ever builds without int128, that fallback is an upstream bug to fix, not to replicate.
- **RV64A via helpers**: LR/SC/AMO .w/.d (IR kinds 62-63, imm packs funct5 | pc_off≪8) call two new guest exports `c2w_jit_amo_w/d` mirroring the interpreter's OP_A macro case-for-case (LR sets load_res, SC compares-without-clearing, AMOs RMW with rd = sign-extended old value; aq/rl ignored like `insn >> 27`). Same wasm→wasm import wiring and fault convention as Batch 4 loads/stores; the atomics pair is optional at helper-resolution time so pre-4.5 artifacts still run under the new jit-host. Scanner rejects LR with rs2≠0 and reserved funct5 (interpreter raises illegal); the rd==x0 short-circuit now excludes 0x2f (an AMO performs its RMW regardless of rd).
- Debug knobs `JIT_NO_MULDIV=1` / `JIT_NO_AMO=1`; 5 new semantic tests ([jit-host/tests/muldiv_amo.rs](jit-host/tests/muldiv_amo.rs)) — the full MulDiv kind matrix vs an i128 Rust reference over 14 edge-case operand pairs, AMO args/wiring/funct5/x0-sentinel, fault tag + skip-rest, mid-block scratch-local threading.

**Numbers (3-run interleaved medians, same session; jit6 = this batch)**:

| Workload | jit0 (true interp) | jit5 (Batch 5 default) | **jit6 (M+A)** |
|---|---|---|---|
| busybox echo hi | 2.01s | 3.38s | **2.88s** |
| node hello | 10.34s | 14.27s | **12.87s** (beats jit5 in all 3 rounds) |
| 1e8 arith loop | 70.07s | 69.96s | **70.40s** (tie) |

All outputs correct, reg_fail=0. Block-entry hit rate on hello 64.9% → 67.2%; unique blocks drop (687 → 625) while entries hold — blocks got longer, as intended.

**The load-bearing finding — coverage's wallclock ceiling**: loop insn coverage rose 48.3% → 54.7% (interpreted insns 2.36B → 2.07B of jit0's 4.56B total) with **zero wallclock change**. With compiled code at per-insn parity with the interpreter (Batch 5's correction), moving insns from interpreted to compiled is wallclock-neutral; the hello/echo wins came from longer blocks ⇒ fewer dispatch misses and shorter scan/compile tails, not from faster execution of the covered insns. Adding further coverage terminators (FP, CSR) without first attacking per-entry overhead or compile cost would mostly re-prove this. On hello, coverage barely moved at all (59.8% → 60.7%) — M/A insns there were sprinkled through already-compiled regions, not block-killers.

**Verdict**: keep (strictly ≥ Batch 5 on every workload; vs jit0: hello 0.80×, echo 0.70×, loop parity). Next levers unchanged and now sharper:
1. **Block chaining / longer regions** — at ~4-6 insns/block and ~2B entries per loop run, entry overhead is the loop's whole remaining story.
2. **Async/background compile** — echo's gap to jit0 is still mostly cranelift-on-critical-path (~2.2-2.4s compile_ms on hello-class boots).
3. **Browser-worker port checkpoint** (overdue since Batch 3) before more wasmtime-specific tuning — both remaining levers shape differently there (JS host boundary, Liftoff tiering).

#### Batch 8 result (2026-06-11): block chaining lands correct — wins only the hot loop (−3%), loses hello/echo to compile + miss-path overhead; shelved to opt-in (`JIT_CHAIN=1`)

**What was built** (tinyemu-c2w `d607607`, container2wasm this commit): compiled blocks tail-call each other instead of returning to the C dispatch loop on every transition. The exit epilogue (~60 wasm ops, once per block) probes the guest's own pc→slot map in linear memory — replicating `jit_map_lookup`'s pc + global/page/user gen checks bit-for-bit — and on a validated hit `return_call_indirect`s through the imported funcref table (frame replaced, so chains can't grow the wasm stack). Misses/stale gens/fault bails return to the C loop as before. Correctness pillars:
- **Cycle accounting moves into the blocks**: each block decrements `s->n_cycles` by its own insn count at entry ("self-charge") because chained blocks never return to the hook. `GET_INSN_COUNTER() = insn_counter_addend - s->n_cycles`, so guest virtual time stays exact mid-chain. The scanner's true insn count (NOT derivable from IR — x0-write nops emit no tuple) rides a new 5-param `register_block2` import (old 4-param name kept registered for pre-Batch-8 artifacts) and is folded into the re-link hash + content-cache key (two byte sequences can yield identical IR with different counts via 2/4-byte nop mixes).
- **ABI handshake**: layout selectors 7-19 expose map base / entry layout / gen-counter + n_cycles addresses; selector 20 is an ack with a side effect — host calls it iff it emits self-charging blocks, setting `c2w_jit_selfcharge` so the hook stops charging. Old host + new guest, new host + old guest, and `JIT_CHAIN` unset all degrade cleanly to Batch 4.5 behavior.
- **Interrupt latency preserved**: epilogue re-checks `n_cycles <= 0` before every hop and bails to the loop on exhaustion. `mip` can't newly assert mid-chain from guest action (CSR writes/fence are scan terminators) except via MMIO stores through helpers (e.g. CLINT msip) — those get serviced at budget exhaustion, an architecturally legal asynchronous-interrupt delay.
- 6 semantic tests ([jit-host/tests/chain.rs](jit-host/tests/chain.rs)): hop executes target + exact two-block self-charge, budget exhaustion returns without hop, stale user/global/page gens each fall back, kernel-half pcs skip the user-gen check, fault path never probes the map, no-chain mode emits the legacy shape.

**Numbers (3-run interleaved medians, same session; nochain = same jit7c artifact with chaining off)**:

| Workload | jit0 | jit6 (B4.5) | jit7c chain | jit7c nochain |
|---|---|---|---|---|
| busybox echo hi | 2.41s | 3.99s | 4.36s | 4.08s |
| node hello | 12.64s | 13.70s | **15.14s** | 13.66s |
| 1e8 arith loop | 78.39s | 87.01s | **86.45s** | 89.37s |

All outputs correct, reg_fail=0. Chain beats nochain on the loop in 3/3 rounds (−2.9s median at 1.0B hops) and loses hello in 3/3 (+1.5s at 74.7M hops). Decomposition: the avoided C-loop round trip is worth ~3.6ns/hop, but chain mode costs +15-20% cranelift time (epilogue ops; hello compile_ms 2.86→3.42s) plus probe-work on every block exit that misses — on boot-heavy workloads the misses + compile outweigh the hops. **Batch 5's lesson generalizes: under wasmtime/cranelift, per-transition micro-savings (a few ns) lose to any per-block code-size/compile cost.** Dispatch round trips and cross-module calls are simply not expensive enough here to pay for inlining anything.

**Verdict**: opt-in via `JIT_CHAIN=1` (like `JIT_INLINE_TLB`), fully tested and kept working. Re-evaluate (a) once async compile takes cranelift off the critical path — chaining's loop win would then stand alone, and (b) at browser-port time, where compile (Liftoff) is cheaper and call costs differ. The "longer regions" idea survives in a different form: superblock/trace compilation (fewer, longer blocks) attacks entry overhead without per-exit probe work — but it's Tier 3 phase 2 territory.

**Consequence for the plan**: with coverage (B4.5) and transition cost (B8) both measured to their ceilings, **async/background compile is now unambiguously the top lever** — hello's entire 1.0s gap to jit0 and most of echo's 1.6s gap is cranelift-on-critical-path. After that, the browser-worker port checkpoint (now 5 batches overdue) before any further wasmtime-side tuning.

#### Browser-port checkpoint result (2026-06-11, evening): JIT runs correct in V8 — and the two wasmtime-shelved optimizations flip to wins. First config to beat the interpreter: inline TLB (+ chaining) in the browser

**What was built** (committed this session; runtime scaffold lives in /tmp/out-browser, recipe in [browser-bench/README.md](browser-bench/README.md)):
- **[jit-codegen-wasm/](jit-codegen-wasm/)** — the existing jit-host codegen compiled to wasm32-unknown-unknown via `#[path]` includes of the same codegen.rs/ir.rs (byte-identical block output, no hand-port), behind a C-ABI surface (`jcg_in_ptr`/`jcg_build`/`jcg_out_ptr`/`jcg_set_tlb`/`jcg_set_chain`). 173KB, no wasm-bindgen.
- **JS coordinator** ([browser-bench/htdocs/worker.js](browser-bench/htdocs/worker.js)) — provides the `jit.*` imports in the scaffold worker. `register_block2`: read IR from guest memory → content cache (pc, end_pc, n_insns, IR bytes) → codegen wasm → **sync `new WebAssembly.Module`** (legal in workers) → instantiate against the guest's exported memory/`__indirect_function_table`/helper funcs (named imports — no positional juggling) → `table.grow`+`table.set` → return slot. `flush_blocks`/`mark_uncompilable` are stats-only counters — the gen-counter invalidation design (Batch 3.5) needed zero changes for the JS host, as intended. `c2w_blk.read/write` stubbed returning −1. Layout selectors 0-6 (TLB) and 7-20 (chain + ack) queried pre-`_start` exactly like main.rs. URL params: `cmd`/`cmd64` (container command), `img` (artifact), `jit=off`, `chain=on`, `tlb=on`.
- **Bench harness** — page auto-POSTs one result JSON per run (wallclock split fetch/instantiate/run, compile counters, chainHops read from guest memory at exit, output tail for correctness) to [browser-bench/collector.py](browser-bench/collector.py) on :8081; chains through suite.json steps with a watchdog. Fully autonomous A/B loops.
- **Driving**: full Chrome (system + Playwright-bundled) hangs at startup in this WSL2 — `chrome-headless-shell` works and is real V8 (same Liftoff/TurboFan tiering), so all numbers below are headless-shell. Page also works in headed Chrome (user-verified window).

**Numbers** (medians, n=2-4 interleaved rounds, same box/session; artifacts: jit0 = true interp, jit7c = Batch 8; all 32 runs output-correct, exit 0, regFail=0):

| Config | echo hi | node hello | 1e8 loop |
|---|---|---|---|
| interp (jit0) | **1.50s** | **11.73s** | 76.92s |
| jit (default = helper-call mem, no chain) | 1.94s | 12.62s | 80.48s |
| jit + tlb=on | — | 12.29s | 73.39s |
| jit + chain=on | — | 12.43s | 78.90s |
| jit + chain+tlb | — | 12.81s | **72.75s** |

Median compile cost: echo 0.20s, hello 0.39s, loop 0.55-0.60s. Browser interp ≈ wasmtime interp (hello 11.7 vs 10-13; loop 76.9 vs 68-78). Browser JIT-config numbers are strikingly stable (loop-tlb: 73.2/73.3/73.5/73.7 across 4 runs spanning 20 min) while interp drifted 75.1→82.5 — compiled-block timing is evidently less sensitive to whatever WSL2 box state does to the interpreter.

**The three load-bearing findings**:
1. **V8 compile is ~5× cheaper than cranelift** (~0.18ms vs ~0.85ms per module; hello total compile 0.39s vs 2.86s). The "async compile is the top lever" conclusion from Batch 8 is **wasmtime-specific** — in the browser (the actual product target) compile-on-critical-path is already a non-issue. Async compile drops from "unambiguously next" to "wasmtime-only nicety".
2. **Inline TLB flips from net-loss to the first JIT-beats-interpreter result** (loop 73.4 vs 76.9 interp; also best single config on hello). V8's cross-instance wasm→wasm calls are expensive enough that inlining the probe pays — the exact reason Batch 5 kept the code opt-in for browser re-evaluation. **Chaining also flips** on the loop (and stacks: chain+tlb 72.75 = best, 1.02B hops engaged) though it hurts boot-heavy hello when combined (12.81 vs tlb-alone 12.29 — epilogue probe-misses on cold code). **Browser defaults should be: tlb=on always, chain=on for compute-heavy** (wasmtime defaults unchanged).
3. **Honest overall state in the target environment**: best-config JIT beats interp ~5% on the hot loop, loses ~4% on hello and ~30% on echo. The remaining echo/hello gap is *not* compile (0.2-0.4s) — it's miss-path overhead (map+gate lookups on once-through boot code) and compiled code still running at ≤ interpreter parity per-insn under V8 too. Same per-insn-parity wall as wasmtime: getting past it needs better compiled code (register lifting / Batch 6, superblocks), not cheaper transitions — those are now cheap everywhere.

**Next levers, reordered for the browser target**: (1) per-insn quality of compiled code — Batch 6 register lifting is unparked now that compile is cheap and blocks are 4-6 insns with TLB inlined; (2) superblock/longer regions to amortize entry overhead; (3) miss-path cost on boot code (cheaper gate/map probe, or higher JIT_HOT_THRESHOLD in browser builds). Wasmtime-side async compile is deprioritized — measure in browser first from now on, since both backends have now disagreed twice (TLB, chaining) on what's worth doing.

#### Batch 6 result (2026-06-13): register lifting lands correct — and measures performance-NEUTRAL in the browser. The per-insn-parity wall's mechanism is now identified: the wasm backend already does this. Shelved to opt-in (`JIT_LIFT=1` / `lift=on`)

**What was built** (container2wasm this commit; **no tinyemu-c2w change** — lifting is pure host/codegen, so it A/Bs on the existing jit7c artifact): compiled blocks keep each non-helper-written guest register in a wasm **local** for the block's lifetime instead of re-loading/storing it to the RISCVCPUState in linear memory around every op. `analyze_lift` in [jit-host/src/codegen.rs](jit-host/src/codegen.rs) walks the block IR and assigns a local per used register; `load_reg`/`store_reg`/`begin_store` hit the local when lifted, memory otherwise; `emit_entry_loads` primes locals at entry, `spill` writes written locals back at every exit. Threaded through the wasm codegen crate ([jit-codegen-wasm/src/lib.rs](jit-codegen-wasm/src/lib.rs) `jcg_set_lift`), the host ([jit-host/src/main.rs](jit-host/src/main.rs) `JIT_LIFT` env), and the browser worker ([browser-bench/htdocs/worker.js](browser-bench/htdocs/worker.js) `lift=on`). Default OFF; `lift_flag=false` yields an empty `Lift` and byte-identical codegen to before.

**Three correctness invariants, all enforced + tested** (see the `Lift` doc comment):
1. Registers written by a memory/AMO **helper** (load.rd, amo.rd) are never lifted — the C helper writes guest memory directly (confirmed: helpers take addr/val/rd_off as args and touch registers *only* via rd writeback), so a lifted local would diverge across the call. Excluded via `op_helper_write`.
2. Every written lifted register is spilled to memory at **every** exit — normal end, chain hand-off (spill runs before `chain_epilogue`), and the **mid-block MMU-fault bail** (`fault_check` spills before returning the tagged pc, so the interpreter re-executes the faulting insn against fresh registers).
3. Any register spillable at a fault is entry-loaded (so its local holds the live memory value even when its own write is after the fault point); fault-free blocks skip that and only entry-load read-before-write registers, keeping hot ALU loops tight. Offset 0 is the x0-discard sentinel (loads/AMO to x0 emit rd_off=0; real regs start at offsetof(reg[0])) and is naturally excluded by the `rd != 0` guards.

**Correctness verification (exhaustive)**: 4 new executing tests in [jit-host/tests/lift.rs](jit-host/tests/lift.rs) — intra-block value flow (reg written→re-read→re-written threads through its local), **fault-spills-earlier-writes** (the load-bearing one: a faulting load commits the prior op's write and the trailing op does not run), reg-survives-helper-call (lifted reg held across a successful helper load whose dest is read back from memory), and a lift==no-lift differential over signed/unsigned inputs. Plus: 17 existing codegen tests still green (lift-off byte-identical); a **7-lens adversarial-review workflow** (exit-spill completeness, fault-stale-local, helper divergence, stack balance, local-index collision, lift-off differential, x0/aliasing — each finding double-verified by skeptics) returned **0 confirmed findings**; and `reg_fail=0` with correct output on the real artifact under both wasmtime (`JIT_LIFT=1`) and **every one of 57 browser runs** (33-step full suite + 24-step focused suite).

**Numbers — browser (the target; wasmtime is correctness pre-flight only)**. Focused 6-round same-box A/B (loop-tlb box spread only 2.8% — a rare stable session; the WSL2 box otherwise drifts 50%+, so only within-round lift/baseline ratios are trusted):

| Workload | lift/baseline (tlb on) median | per-round ratios |
|---|---|---|
| 1e8 arith loop | **1.000** | 0.988, 1.044, 1.013, 1.004, 0.996, 0.990 |
| node hello | **1.008** | 0.995, 1.008, 1.015, 1.008, 1.009 |

The loop is *exactly* at parity (no consistent sign); hello is a consistent ~0.8% **slower** (entry/spill overhead on boot code with little reuse). The earlier 33-step full suite agreed (within-round medians 0.976–0.996 across echo/hello/loop). **Register lifting is performance-neutral — no win.**

**The load-bearing finding — why, and what it means for the plan**: lifting was the plan's designated attack on the per-insn-parity wall (original Batch 6 hoped 1.2–1.5×). It produced ~0×. The mechanism: **the wasm backend (V8 TurboFan / cranelift) already does intra-block register allocation and store-to-load forwarding** over our fixed-offset state-pointer accesses. The "naive" codegen's repeated `i64.load/store [state+off]` are alias-analyzable, so the engine already keeps them in machine registers across the block — explicit lifting to wasm locals is doing a job the backend was already doing. It *should* help in exactly one place — blocks with **helper calls** (loads/stores), where the backend must assume the opaque call clobbers memory and reload guest regs after it, whereas wasm locals survive calls — but those are boot-heavy blocks dominated by compile/miss-path cost, so the theoretical win never surfaces. And the hot ALU loop has no calls, so the backend already register-allocates it fully ⇒ lifting redundant exactly where per-insn perf matters.

**Consequence — per-insn codegen quality is exhausted as a lever**: this measures out the *last* "make each compiled instruction cheaper" idea (after inline-TLB B5 and the cost-model corrections). Under both backends the engine already extracts intra-block per-insn quality; we can't beat it by hand. The remaining levers are **structural, not per-op**: (1) superblock/trace compilation (longer regions — amortize the ~per-block entry/dispatch overhead AND raise register reuse so lifting could finally pay, since across many basic blocks the backend can't see the whole region), (2) miss-path/gate cost on boot code (cheaper map probe or higher JIT_HOT_THRESHOLD in browser builds), (3) the deferred snapshot/warm-pool workstream for cold-start (the only lever that helps echo-class agent shell-outs, which no JIT can win). Register lifting stays **tested-and-shelved** (opt-in like inline-TLB and chaining) because it becomes correct-and-relevant infrastructure once superblocks raise reuse, or on an engine with costly memory access.

**Tooling added** (all committed): `browser-bench/serve/` is now a **persistent** (gitignored) serve dir replacing the reboot-wiped `/tmp/out-browser`, rebuilt by [browser-bench/rebuild-serve.sh](browser-bench/rebuild-serve.sh); [browser-bench/collector.py](browser-bench/collector.py) writes results there (not /tmp); [browser-bench/gen-suite.py](browser-bench/gen-suite.py) gained `--quick` (loop-only, 2e7-iter, 3-pair ~1.5min iteration harness) alongside the full 33-run commit-gate suite. Iteration loop going forward: wasmtime CLI correctness (~10s, reg_fail) → `gen-suite.py --quick` browser perf (~1.5min) → full suite only for the final verdict.

---

## Project status (2026-06-13): JIT path concluded; next direction = guest SMP; PARKED on team capacity

**Active development paused.** Decision after working through the JIT results and the multicore question end-to-end. Resume target: ~end 2026, or whenever a materially more capable model is available (see "capability context" below).

### Where the JIT path landed (concluded)

Batches 1–8 + register lifting (Batch 6) are done. **Net result: an in-wasm BB JIT lands at roughly parity-to-+5% vs the interpreter on hot loops in the browser, and *loses* on boot-heavy / once-through code (~30% on echo).** Root cause is now fully understood and is structural, not a tuning gap:

- A **wasm-emitting JIT cannot beat a wasm-compiled interpreter by much**, because the same backend (V8/cranelift) compiles both. (a) TinyEMU's interpreter is already compiled C→wasm→native, so the baseline isn't a slow dispatch loop — it's lean machine code (~7ns/insn). (b) We can only emit *wasm*, which V8 re-JITs and already register-allocates + store-to-load-forwards. So per-instruction codegen quality (inline-TLB B5, register lifting B6) is **redundant — the engine already does it** (both measured neutral). Compiled code runs at per-insn *parity*; moving insns interp→compiled is wallclock-neutral (B4.5).
- The classic 5–10× emulator-JIT win requires emitting **native** code (QEMU TCG → x86), which removes a layer. The browser sandbox forbids that — you can only emit wasm into a wasm engine that re-JITs it, and the double layer cancels the win. **It's the wasm-sandbox target that defeats the JIT, not RISC-V or emulation.**
- **Only remaining JIT lever: superblocks/traces** — hand V8 *bigger regions* (whole hot traces as one module) so it optimizes across basic blocks it currently can't see whole (the one place register lifting *would* finally pay). Bounded ~2–4× on hot code only, and it helps the hot-loop case — **not** the boot/fork-bound shell-out workload that is the actual product. Not started.

### Next real lever (the parked work): guest SMP / multi-core

The interpreter is single-hart: one host thread runs one emulated CPU; the entire guest (kernel + all processes) time-slices onto it. Giving the guest **SMP** is the next structural lever.

- **Design**: one Web Worker per emulated hart, shared guest RAM via SharedArrayBuffer (already in use). The **threading is free** — a Worker is an OS thread, the host OS spreads Workers across physical cores automatically; guest Linux booted with N harts schedules its processes across them.
- **Why it's expensive (the whole cost is correctness)**: cross-hart atomics (LR/SC, AMO) mapped onto JS `Atomics.*` on the SAB; the RISC-V **weak memory model (RVWMO)** and `fence` semantics emulated correctly on top of the JS/wasm memory model (the killer — subtle bugs = rare non-reproducible corruption); per-hart CSRs/MMU/TLB/timers; CLINT/PLIC **IPIs** + cross-hart **TLB shootdowns**; making the JIT block-cache/generation-invalidation and virtio/console device models concurrency-safe. TinyEMU has **zero SMP support today** — this is a concurrency rewrite of its core. (QEMU's equivalent, MTTCG, took years.)
- **Gating question — MEASURE BEFORE BUILDING**: SMP only helps when the guest workload is *genuinely parallel* (concurrent guest processes / multi-threaded programs). A single command or serial boot/fork doesn't parallelize, and **fork/exec is kernel-lock-heavy**, so even concurrent guest processes may scale sub-linearly (the guest kernel's mm locks become real cross-hart contention — fork storms don't scale on real hardware either). So first measure: *what fraction of a real agent session is concurrent guest processes vs. one-thing-at-a-time serial boot/fork/single-command?* That ratio decides whether SMP is transformative or just idle cores. (This is the same realistic-agent-workload benchmark CLAUDE.md has flagged as unmeasured since Tier 1.)

### The QEMU-wasm alternative (already has SMP)

container2wasm's **QEMU backend** gets multi-core "out of the box" because it inherits both hard parts: QEMU's **MTTCG** (years of multi-vCPU correctness work) + Emscripten's `pthreads` (which *are* Web Workers over shared memory). But QEMU-wasm is heavier and slower per-instruction (measured ~22s jitless hello, futex stalls) — the reason TinyEMU was chosen. So the real fork is empirical: **N-cores-slow (QEMU) vs 1-core-fast (TinyEMU)** for the actual parallel-process content of an agent session. Two things to verify when revisiting: (1) whether qemu-wasm's *browser* build runs true MTTCG (parallel vCPUs) or round-robin RR (multiple threads serialized behind a global lock — Workers visible but no speedup); (2) the same workload-parallelism ratio above.

### Decision & plan

Guest SMP is **too large for current capacity** (one dev ~a few hours/day + one AI agent) and the per-instruction JIT path has hit its structural ceiling. So: **park active development.** When resuming (more capable model and/or more time): first build the realistic-agent-workload benchmark and measure the concurrency ratio; if the workload is parallel-process-heavy, attempt either TinyEMU SMP (the rewrite) or re-evaluate QEMU-wasm (already has it); if it's serial/boot-bound, neither SMP nor more JIT helps and the lever is snapshot/warm-pool cold-start (or the WASIX pivot). Default plan unless a future model finds a better direction first: **attempt the TinyEMU multi-core / Worker-per-hart setup.**

### Capability context

A recent stronger model (Fable / Mythos 5) gave a real productivity boost on this work but is **no longer available (withdrawn per US request)**. This is correctness-heavy systems work where model capability is the binding constraint on a tiny team, so resumption is gated on a more capable model returning/arriving. Revisit ~half a year out.
