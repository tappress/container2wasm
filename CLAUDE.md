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
