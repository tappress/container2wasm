//! Semantic tests for Batch 4.5 codegen.
//!
//! RV64M: every op kind is run against a Rust reference implementing the
//! RISC-V semantics (i128 for the mulh family, explicit zero-divisor /
//! overflow rules for div/rem) over an edge-case matrix — no helpers, the
//! result is read back from the register file in linear memory.
//!
//! RV64A: the c2w_jit_amo_* helpers are replaced by host funcs that record
//! their arguments, verifying import wiring, argument derivation
//! (addr = reg[rs1] with no displacement, val2 = reg[rs2], funct5 and rd_off
//! passthrough, x0 sentinel) and the fault path (tagged pc, later ops
//! skipped).

use jit_host::codegen;
use wasmtime::*;

const REG_BASE: usize = 16;
fn reg_off(n: usize) -> usize {
    REG_BASE + n * 8
}

fn push(v: &mut Vec<u8>, kind: u8, rd: u16, rs1: u16, rs2: u16, imm: i64) {
    v.push(kind);
    v.push(0);
    v.extend_from_slice(&rd.to_le_bytes());
    v.extend_from_slice(&rs1.to_le_bytes());
    v.extend_from_slice(&rs2.to_le_bytes());
    v.extend_from_slice(&imm.to_le_bytes());
}

/// RISC-V reference semantics keyed by IR kind (49..=61).
fn reference(kind: u8, a: u64, b: u64) -> u64 {
    let (sa, sb) = (a as i64, b as i64);
    match kind {
        49 => sa.wrapping_mul(sb) as u64,                                  // mul
        50 => (((sa as i128) * (sb as i128)) >> 64) as u64,                // mulh
        51 => (((sa as i128) * (b as i128)) >> 64) as u64,                 // mulhsu
        52 => (((a as u128) * (b as u128)) >> 64) as u64,                  // mulhu
        53 => {
            // div
            if sb == 0 {
                u64::MAX
            } else if sa == i64::MIN && sb == -1 {
                a
            } else {
                (sa / sb) as u64
            }
        }
        54 => if b == 0 { u64::MAX } else { a / b },                       // divu
        55 => {
            // rem
            if sb == 0 {
                a
            } else if sa == i64::MIN && sb == -1 {
                0
            } else {
                (sa % sb) as u64
            }
        }
        56 => if b == 0 { a } else { a % b },                              // remu
        57 => (a as i32).wrapping_mul(b as i32) as i64 as u64,             // mulw
        58 => {
            // divw
            let (x, y) = (a as i32, b as i32);
            let r = if y == 0 {
                -1
            } else if x == i32::MIN && y == -1 {
                x
            } else {
                x / y
            };
            r as i64 as u64
        }
        59 => {
            // divuw
            let (x, y) = (a as u32, b as u32);
            let r = if y == 0 { u32::MAX } else { x / y };
            r as i32 as i64 as u64
        }
        60 => {
            // remw
            let (x, y) = (a as i32, b as i32);
            let r = if y == 0 {
                x
            } else if x == i32::MIN && y == -1 {
                0
            } else {
                x % y
            };
            r as i64 as u64
        }
        61 => {
            // remuw
            let (x, y) = (a as u32, b as u32);
            let r = if y == 0 { x } else { x % y };
            r as i32 as i64 as u64
        }
        _ => unreachable!(),
    }
}

/// Build a one-op MulDiv block (rd = x10, rs1 = x11, rs2 = x12), run it with
/// the given operand values, return reg[x10] afterwards.
fn run_muldiv(kind: u8, a: u64, b: u64) -> u64 {
    let mut ir = Vec::new();
    push(
        &mut ir,
        kind,
        reg_off(10) as u16,
        reg_off(11) as u16,
        reg_off(12) as u16,
        0,
    );
    let (bytes, used) = codegen::build_block(&ir, 0x1000, 0x1004, None);
    assert!(used.is_empty(), "muldiv blocks import no helpers");
    let engine = Engine::default();
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    let mut store: Store<()> = Store::new(&engine, ());
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    for (n, val) in [(11, a), (12, b)] {
        let off = reg_off(n);
        mem.data_mut(&mut store)[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem)]).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let pc = f.call(&mut store, 0).unwrap();
    assert_eq!(pc, 0x1004, "no terminator: falls through");
    let snap = mem.data(&store);
    u64::from_le_bytes(snap[reg_off(10)..reg_off(10) + 8].try_into().unwrap())
}

#[test]
fn muldiv_matches_riscv_reference() {
    let cases: &[(u64, u64)] = &[
        (0, 0),
        (1, 0),
        (u64::MAX, 0),
        (i64::MIN as u64, u64::MAX),     // MIN / -1 (64-bit overflow rule)
        (0xdead_beef_8000_0000, u64::MAX), // W-form MIN32/-1 with dirty upper bits
        (7, 3),
        ((-7i64) as u64, 3),
        (7, (-3i64) as u64),
        ((-7i64) as u64, (-3i64) as u64),
        (0x1234_5678_9abc_def0, 0xfedc_ba98_7654_3210),
        (u64::MAX, u64::MAX),
        (1u64 << 63, 2),
        (0xffff_ffff_0000_0001, 0x0000_0001_ffff_ffff),
        (0x8000_0000, 0x8000_0000), // INT32_MIN in low halves, positive as u64
    ];
    for kind in 49..=61u8 {
        for &(a, b) in cases {
            let got = run_muldiv(kind, a, b);
            let want = reference(kind, a, b);
            assert_eq!(
                got, want,
                "kind {kind} a={a:#x} b={b:#x}: got {got:#x} want {want:#x}"
            );
        }
    }
}

/// MulDiv ops are correct mid-block too (operand/result threading through
/// the shared scratch locals doesn't disturb neighbours): x10 = x11 mulhu
/// x12, then x13 = x10 + 1 via ADDI, then x14 = x11 rem x12.
#[test]
fn muldiv_mid_block_threading() {
    let mut ir = Vec::new();
    push(&mut ir, 52, reg_off(10) as u16, reg_off(11) as u16, reg_off(12) as u16, 0);
    push(&mut ir, 2, reg_off(13) as u16, reg_off(10) as u16, 0, 1); // addi x13, x10, 1
    push(&mut ir, 55, reg_off(14) as u16, reg_off(11) as u16, reg_off(12) as u16, 0);
    let (bytes, used) = codegen::build_block(&ir, 0x2000, 0x200c, None);
    assert!(used.is_empty());
    let engine = Engine::default();
    let module = Module::from_binary(&engine, &bytes).unwrap();
    let mut store: Store<()> = Store::new(&engine, ());
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    let (a, b) = (0xfedc_ba98_7654_3210u64, 0x1234_5678_9abc_def0u64);
    for (n, val) in [(11, a), (12, b)] {
        let off = reg_off(n);
        mem.data_mut(&mut store)[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem)]).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    assert_eq!(f.call(&mut store, 0).unwrap(), 0x200c);
    let snap = mem.data(&store);
    let r = |n: usize| u64::from_le_bytes(snap[reg_off(n)..reg_off(n) + 8].try_into().unwrap());
    assert_eq!(r(10), reference(52, a, b));
    assert_eq!(r(13), reference(52, a, b).wrapping_add(1));
    assert_eq!(r(14), reference(55, a, b));
}

#[derive(Default)]
struct AmoCalls {
    calls: Vec<(u64, u64, u32, u32)>, // (addr, val2, funct5, rd_off)
    fail: bool,
}

/// Run a block whose AMO helpers are host-recorded. Returns (next_pc, calls).
fn run_amo(ir: &[u8], regs: &[(usize, u64)], fail: bool, expect_used: &[usize]) -> (i64, AmoCalls) {
    let (bytes, used) = codegen::build_block(ir, 0x3000, 0x3000 + 4 * 4, None);
    assert_eq!(used, expect_used, "helper import set");
    let engine = Engine::default();
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    let mut store: Store<AmoCalls> = Store::new(&engine, AmoCalls { fail, ..Default::default() });
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    for &(n, val) in regs {
        let off = reg_off(n);
        mem.data_mut(&mut store)[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
    let amo = Func::wrap(
        &mut store,
        |mut c: Caller<'_, AmoCalls>,
         _state: i32,
         addr: i64,
         val2: i64,
         funct5: i32,
         rd_off: i32|
         -> i32 {
            c.data_mut()
                .calls
                .push((addr as u64, val2 as u64, funct5 as u32, rd_off as u32));
            i32::from(c.data().fail)
        },
    );
    let mut externs = vec![Extern::Memory(mem)];
    for _ in &used {
        externs.push(Extern::Func(amo));
    }
    let inst = Instance::new(&mut store, &module, &externs).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let pc = f.call(&mut store, 0).unwrap();
    (pc, store.into_data())
}

/// amoadd.w x14, x12, (x10) then amoswap.d x0, x12, (x11): args, widths and
/// the x0 sentinel all arrive at the helpers intact.
#[test]
fn amo_args_and_wiring() {
    let mut ir = Vec::new();
    // kind 62 = .w; funct5 = 0 (amoadd), pc_off 0
    push(&mut ir, 62, reg_off(14) as u16, reg_off(10) as u16, reg_off(12) as u16, 0);
    // kind 63 = .d; funct5 = 1 (amoswap), rd = x0 sentinel, pc_off 4
    push(&mut ir, 63, 0, reg_off(11) as u16, reg_off(12) as u16, 1 | (4 << 8));
    let (pc, calls) = run_amo(
        &ir,
        &[(10, 0x8000), (11, 0x9000), (12, 77)],
        false,
        &[11, 12],
    );
    assert_eq!(pc, 0x3010, "falls through to end_pc");
    assert_eq!(
        calls.calls,
        vec![
            (0x8000, 77, 0, reg_off(14) as u32),
            (0x9000, 77, 1, 0),
        ]
    );
}

/// LR.W (funct5=2) and SC.W (funct5=3) ride the same IR kind.
#[test]
fn amo_lr_sc_funct5_passthrough() {
    let mut ir = Vec::new();
    push(&mut ir, 62, reg_off(14) as u16, reg_off(10) as u16, reg_off(0) as u16, 2);
    push(&mut ir, 62, reg_off(15) as u16, reg_off(10) as u16, reg_off(12) as u16, 3 | (4 << 8));
    let (pc, calls) = run_amo(&ir, &[(10, 0x8000), (12, 42)], false, &[11]);
    assert_eq!(pc, 0x3010);
    assert_eq!(calls.calls[0], (0x8000, 0, 2, reg_off(14) as u32));
    assert_eq!(calls.calls[1], (0x8000, 42, 3, reg_off(15) as u32));
}

/// A failing AMO helper bails with the tagged fault pc and skips later ops.
#[test]
fn amo_fault_bails_with_tagged_pc() {
    let mut ir = Vec::new();
    push(&mut ir, 62, reg_off(14) as u16, reg_off(10) as u16, reg_off(12) as u16, 0 | (4 << 8));
    push(&mut ir, 63, reg_off(15) as u16, reg_off(11) as u16, reg_off(12) as u16, 1 | (8 << 8));
    let (pc, calls) = run_amo(&ir, &[(10, 0x8000), (11, 0x9000), (12, 7)], true, &[11, 12]);
    assert_eq!(pc as u64, (0x3000 + 4) | 1, "fault = (start_pc + pc_off) | 1");
    assert_eq!(calls.calls.len(), 1, "ops after the faulting AMO must not run");
}
