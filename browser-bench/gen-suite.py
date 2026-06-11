#!/usr/bin/env python3
"""Generate suite.json for the browser benchmark chain.

Each entry is a query string for the scaffold page; index.html runs the step,
POSTs the result to collector.py on :8081, then navigates to the next step.
Page params: cmd / cmd64 (container command), img (artifact, default out.wasm),
jit=off, chain=on, tlb=on, post=<tag>, step=<n>, watchdog=<ms>.

Usage: ./gen-suite.py > /path/to/htdocs/suite.json
Then open http://localhost:8080/?<first entry> (or drive it with
chrome-headless-shell; full Chrome hangs under WSL2, headless-shell works).
"""
import base64
import json
import sys
import urllib.parse


def c64(args):
    return "cmd64=" + urllib.parse.quote(base64.b64encode(json.dumps(args).encode()).decode())


HELLO = c64(["node", "-e", "console.log('R', 1+1)"])
LOOP = c64(["node", "-e", "let s=0;for(let i=0;i<1e8;i++)s+=i*7&255;console.log('R',s)"])
ECHO = "cmd=echo%20hi"

INTERP = "&img=jit0.wasm&jit=off"  # true-interpreter artifact, coordinator off

steps = []


def add(label, qs, watchdog):
    steps.append(qs + f"&post={label}&step={len(steps)}&watchdog={watchdog}")


for rnd in (1, 2, 3):
    add(f"r{rnd}-echo-interp", ECHO + INTERP, 360000)
    add(f"r{rnd}-echo-jit", ECHO, 360000)
    add(f"r{rnd}-hello-interp", HELLO + INTERP, 1200000)
    add(f"r{rnd}-hello-tlb", HELLO + "&tlb=on", 1200000)
    add(f"r{rnd}-loop-interp", LOOP + INTERP, 3600000)
    add(f"r{rnd}-loop-chaintlb", LOOP + "&chain=on&tlb=on", 3600000)

json.dump(steps, sys.stdout, indent=1)
print(f"\nfirst url: http://localhost:8080/?{steps[0]}", file=sys.stderr)
