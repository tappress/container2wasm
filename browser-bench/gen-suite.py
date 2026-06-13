#!/usr/bin/env python3
"""Generate suite.json for the browser benchmark chain.

Each entry is a query string for the scaffold page; index.html runs the step,
POSTs the result to collector.py on :8081, then navigates to the next step.
Page params: cmd / cmd64 (container command), img (artifact, default out.wasm),
jit=off, chain=on, tlb=on, lift=on, post=<tag>, step=<n>, watchdog=<ms>.

Usage:
  ./gen-suite.py            > htdocs/suite.json   # full commit-gate verdict
  ./gen-suite.py --quick    > htdocs/suite.json   # fast iteration A/B
Then open http://localhost:8080/?<first entry> (or drive it with
chrome-headless-shell; full Chrome hangs under WSL2, headless-shell works).

WHY ROUNDS: this WSL2 box drifts wildly between runs (interp has swung
83s->134s). Only a lift-on vs lift-off pair *within the same round* is
trustworthy — same box state for both. N rounds give N clean ratios whose
median resolves the signal; cross-round absolute times are meaningless.

--quick is the iteration harness: loop-only (the most ALU-dense workload, the
only place a per-insn codegen change like lifting can show), a SHORTER loop
(2e7 iters ~15s vs 1e8 ~80s — same compiled hot block, same lift/baseline
ratio, compile cost still <10%), 3 rounds = 6 runs ~1.5 min. Resolves ~+/-5%,
enough to spot a real win or regression. Drop to the full suite (echo/hello +
chain configs, 3 rounds, 33 runs ~25 min) only for the final verdict. For pure
correctness iteration, skip the browser entirely and use the wasmtime CLI
(JIT_LIFT=1 jit-host ... node -e ...; ~10s, checks reg_fail=0 + output).
"""
import argparse
import base64
import json
import sys
import urllib.parse


def c64(args):
    return "cmd64=" + urllib.parse.quote(base64.b64encode(json.dumps(args).encode()).decode())


def loop(iters):
    return c64(["node", "-e", f"let s=0;for(let i=0;i<{iters};i++)s+=i*7&255;console.log('R',s)"])


HELLO = c64(["node", "-e", "console.log('R', 1+1)"])
LOOP = loop("1e8")
ECHO = "cmd=echo%20hi"
INTERP = "&img=jit0.wasm&jit=off"  # true-interpreter artifact, coordinator off


def build(steps, label, qs, watchdog):
    steps.append(qs + f"&post={label}&step={len(steps)}&watchdog={watchdog}")


def full_suite():
    steps = []
    # Per round, interleave every config so within-round box drift hits all
    # configs equally (only same-round comparisons are trusted).
    for rnd in (1, 2, 3):
        build(steps, f"r{rnd}-echo-interp", ECHO + INTERP, 360000)
        build(steps, f"r{rnd}-echo-tlb", ECHO + "&tlb=on", 360000)
        build(steps, f"r{rnd}-echo-tlblift", ECHO + "&tlb=on&lift=on", 360000)
        build(steps, f"r{rnd}-hello-interp", HELLO + INTERP, 1200000)
        build(steps, f"r{rnd}-hello-tlb", HELLO + "&tlb=on", 1200000)
        build(steps, f"r{rnd}-hello-tlblift", HELLO + "&tlb=on&lift=on", 1200000)
        build(steps, f"r{rnd}-loop-interp", LOOP + INTERP, 3600000)
        build(steps, f"r{rnd}-loop-tlb", LOOP + "&tlb=on", 3600000)
        build(steps, f"r{rnd}-loop-tlblift", LOOP + "&tlb=on&lift=on", 3600000)
        build(steps, f"r{rnd}-loop-chaintlb", LOOP + "&chain=on&tlb=on", 3600000)
        build(steps, f"r{rnd}-loop-chaintlblift", LOOP + "&chain=on&tlb=on&lift=on", 3600000)
    return steps


def quick_suite(rounds, iters):
    steps = []
    qloop = loop(iters)
    # loop-only, lift A/B per round; one shared interp anchor up front for an
    # absolute-drift reference without paying for it every round.
    build(steps, "q-loop-interp", qloop + INTERP, 600000)
    for rnd in range(1, rounds + 1):
        build(steps, f"q{rnd}-loop-tlb", qloop + "&tlb=on", 600000)
        build(steps, f"q{rnd}-loop-tlblift", qloop + "&tlb=on&lift=on", 600000)
    return steps


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--quick", action="store_true", help="fast loop-only lift A/B for iteration")
    ap.add_argument("--rounds", type=int, default=3, help="quick-mode rounds (pairs), default 3")
    ap.add_argument("--iters", default="2e7", help="quick-mode loop iterations, default 2e7")
    args = ap.parse_args()

    steps = quick_suite(args.rounds, args.iters) if args.quick else full_suite()
    json.dump(steps, sys.stdout, indent=1)
    mode = f"quick ({args.rounds} rounds, {args.iters} iters)" if args.quick else "full"
    print(f"\n[{mode}] {len(steps)} steps; first url: http://localhost:8080/?{steps[0]}", file=sys.stderr)


if __name__ == "__main__":
    main()
