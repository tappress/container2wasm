#!/usr/bin/env python3
"""Auto-bisect to find the lowest MAX_BLOCKS that causes the JIT to corrupt.

Usage:
  bisect-bad-block.py <wasm> -- <guest cmd...>
  bisect-bad-block.py --expect "OK" --upper 5000 ./out/alpine-node-jit3.wasm -- \\
      node -e 'console.log("OK", 1+1)'

The script binary-searches MAX_BLOCKS in [0, upper]. It treats a run as
"good" if the host exits 0 AND stdout contains --expect (default: any 0-exit).
"bad" = timeout or non-zero exit or missing expect substring.

Once the boundary is found (largest good = G, smallest bad = G+1), it dumps
block #(G+1)'s IR + decoded wasm and tells you the offending guest PC.
"""

from __future__ import annotations

import argparse
import os
import shutil
import struct
import subprocess
import sys
from pathlib import Path

HOST_BIN = Path(__file__).resolve().parent / "target" / "release" / "jit-host"

# IR op_kind -> mnemonic. Mirror of jit-host/src/ir.rs.
OP_NAMES = {
    1: "Const", 2: "Addi", 3: "Addiw", 4: "Andi", 5: "Ori", 6: "Xori",
    7: "Slti", 8: "Sltiu", 9: "Slli", 10: "Srli", 11: "Srai", 12: "Slliw",
    13: "Srliw", 14: "Sraiw", 15: "Add", 16: "Sub", 17: "And", 18: "Or",
    19: "Xor", 20: "Sll", 21: "Srl", 22: "Sra", 23: "Slt", 24: "Sltu",
    25: "Addw", 26: "Subw", 27: "Sllw", 28: "Srlw", 29: "Sraw",
    30: "Jal", 31: "Jalr", 32: "Beq", 33: "Bne", 34: "Blt", 35: "Bge",
    36: "Bltu", 37: "Bgeu",
}

REG_BASE = 16
REG_STRIDE = 8


def reg_name(off: int) -> str:
    if off == 0:
        return "-"
    if (off - REG_BASE) % REG_STRIDE != 0:
        return f"off={off}"
    idx = (off - REG_BASE) // REG_STRIDE
    return f"x{idx}"


def decode_ir(buf: bytes) -> list[str]:
    out = []
    for i in range(0, len(buf), 16):
        chunk = buf[i:i + 16]
        if len(chunk) < 16:
            break
        op_kind = chunk[0]
        rd_off, rs1_off, rs2_off = struct.unpack_from("<HHH", chunk, 2)
        imm = struct.unpack_from("<q", chunk, 8)[0]
        name = OP_NAMES.get(op_kind, f"OP_{op_kind}")
        out.append(
            f"  [{i // 16:3d}] {name:<6s} rd={reg_name(rd_off):<4s} "
            f"rs1={reg_name(rs1_off):<4s} rs2={reg_name(rs2_off):<4s} "
            f"imm={imm:#x} ({imm})"
        )
    return out


def run_once(args: argparse.Namespace, max_blocks: int, dump_nth: int | None = None) -> tuple[str, str, int]:
    """Returns (status, stdout, exit_code). status in {ok, bad-exit, bad-output, timeout}."""
    env = os.environ.copy()
    env["JIT_MAX_BLOCKS"] = str(max_blocks)
    if dump_nth is not None:
        env["JIT_DUMP_NTH"] = str(dump_nth)
    cmd = [str(HOST_BIN), args.wasm, *args.guest_cmd]
    try:
        p = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
            timeout=args.timeout,
        )
    except subprocess.TimeoutExpired as e:
        out = e.stdout.decode() if isinstance(e.stdout, bytes) else (e.stdout or "")
        return "timeout", out, -1
    if p.returncode != 0:
        return "bad-exit", p.stdout, p.returncode
    if args.expect and args.expect not in p.stdout:
        return "bad-output", p.stdout, p.returncode
    return "ok", p.stdout, p.returncode


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("wasm", help="Path to the c2w wasm built with JIT_DISPATCH_VARIANT=3.")
    ap.add_argument("guest_cmd", nargs=argparse.REMAINDER,
                    help="Guest argv (use -- to separate). e.g. -- node -e 'console.log(1+1)'")
    ap.add_argument("--expect", default="",
                    help="Substring required in stdout for a run to count as good.")
    ap.add_argument("--lower", type=int, default=0, help="Known-good lower bound.")
    ap.add_argument("--upper", type=int, default=1000,
                    help="Initial upper bound. Doubled until a bad run is observed.")
    ap.add_argument("--timeout", type=float, default=90.0,
                    help="Seconds before a run is declared bad-timeout.")
    args = ap.parse_args()

    if args.guest_cmd and args.guest_cmd[0] == "--":
        args.guest_cmd = args.guest_cmd[1:]

    if not HOST_BIN.exists():
        sys.exit(f"jit-host binary missing: {HOST_BIN}. Run `cargo build --release`.")
    if not Path(args.wasm).exists():
        sys.exit(f"wasm missing: {args.wasm}")

    # Step 1: find an upper bound that fails.
    print(f"[bisect] confirming lower={args.lower} is good...", flush=True)
    status, _, _ = run_once(args, args.lower)
    print(f"[bisect]   {args.lower} -> {status}")
    if status != "ok":
        sys.exit(f"[bisect] lower bound {args.lower} is already bad ({status}); pick a smaller --lower or fix prior bugs first.")

    lo, hi = args.lower, args.upper
    while True:
        print(f"[bisect] probing upper={hi}...", flush=True)
        status, _, _ = run_once(args, hi)
        print(f"[bisect]   {hi} -> {status}")
        if status != "ok":
            break
        # Even hi is fine — no corruption in [lo, hi]. Expand.
        lo = hi
        hi *= 2
        if hi > 1_000_000:
            sys.exit("[bisect] could not find a failing MAX_BLOCKS up to 1M; no corruption?")

    # Step 2: binary search for the boundary.
    # Invariant: lo is good, hi is bad.
    while hi - lo > 1:
        mid = (lo + hi) // 2
        print(f"[bisect] probing mid={mid}...", flush=True)
        status, _, _ = run_once(args, mid)
        print(f"[bisect]   {mid} -> {status}")
        if status == "ok":
            lo = mid
        else:
            hi = mid

    bad_n = hi  # 1-indexed: this many blocks registered triggers corruption
    print(f"\n[bisect] BOUNDARY: {lo} good, {hi} bad")
    print(f"[bisect] offending block index: {bad_n} (1-indexed)")

    # Step 3: dump the offending block.
    # JIT_DUMP_NTH=K dumps when n_register_ok == K, i.e., the (K+1)th block.
    # So to dump the bad_n-th (1-indexed) block, set dump_nth = bad_n - 1.
    dump_nth = bad_n - 1
    print(f"[bisect] dumping block #{bad_n} via JIT_DUMP_NTH={dump_nth}...", flush=True)

    # Clean stale dumps with the same idx prefix.
    for stale in Path("/tmp").glob(f"jit_block_n{dump_nth}_*"):
        stale.unlink(missing_ok=True)

    status, stdout, _ = run_once(args, bad_n, dump_nth=dump_nth)
    dumps = sorted(Path("/tmp").glob(f"jit_block_n{dump_nth}_*.ir.bin"))
    if not dumps:
        sys.exit(f"[bisect] no dump produced (status={status}); guest aborted before block #{bad_n} compiled?")

    ir_path = dumps[0]
    wasm_path = ir_path.with_suffix("").with_suffix(".wasm")  # noqa: replace .bin then add .wasm
    # Filename pattern is jit_block_n{idx}_{pc:x}.ir.bin / .wasm
    pc_hex = ir_path.stem.split("_")[-1].removesuffix(".ir")
    wasm_path = Path(f"/tmp/jit_block_n{dump_nth}_{pc_hex}.wasm")

    print(f"\n[bisect] OFFENDING BLOCK #{bad_n} @ guest PC 0x{pc_hex}")
    print(f"[bisect]   IR:   {ir_path} ({ir_path.stat().st_size} bytes)")
    print(f"[bisect]   wasm: {wasm_path} ({wasm_path.stat().st_size} bytes)" if wasm_path.exists() else f"[bisect]   wasm: MISSING ({wasm_path})")

    print("\n--- IR decode ---")
    print("\n".join(decode_ir(ir_path.read_bytes())))

    if wasm_path.exists() and shutil.which("wasm2wat"):
        print("\n--- wasm-wat ---")
        subprocess.run(["wasm2wat", str(wasm_path)])


if __name__ == "__main__":
    main()
