//! Standalone semantic test of the codegen for block n555 (PC 0x11eb0):
//!   addi x14, x25, -1
//!   sub  x22, x0, x25
//!   beq  x24, x0, 0x11fce
//!
//! Fallthrough PC: 0x11ebc.
//!
//! Goal: run the compiled wasm in isolation and verify its return value +
//! register side-effects, without involving the rest of the c2w wasm. If this
//! passes, the wasm-block content is fine and the hang in the full system is
//! caused by something outside the compiled block (e.g., the interpreter's
//! re-entry path after a branch, n_cycles starvation, MMU staleness, etc.).
//!
//! Register-file layout assumed (mirrors c2w's reg_off helper in C):
//!   reg[N] lives at offset 16 + N*8 inside RISCVCPUState.
//! For this test, state_ptr passes as 0 and we use the first ~1KB of guest
//! memory as our scratch state buffer.

use jit_host::codegen;
use wasmtime::*;

const REG_BASE: usize = 16;
const REG_STRIDE: usize = 8;
fn reg_off(n: usize) -> usize {
    REG_BASE + n * REG_STRIDE
}

/// IR bytes for the block (built by hand to mirror jit_codegen.c output —
/// chunks of 16 bytes: kind(u8) pad(u8) rd_off(u16) rs1_off(u16) rs2_off(u16) imm(i64)).
fn ir_block_n555() -> Vec<u8> {
    let mut v = Vec::new();
    let push = |v: &mut Vec<u8>, kind: u8, rd: u16, rs1: u16, rs2: u16, imm: i64| {
        v.push(kind);
        v.push(0);
        v.extend_from_slice(&rd.to_le_bytes());
        v.extend_from_slice(&rs1.to_le_bytes());
        v.extend_from_slice(&rs2.to_le_bytes());
        v.extend_from_slice(&imm.to_le_bytes());
    };
    // [0] Addi rd=x14 rs1=x25 imm=-1
    push(&mut v, 2, reg_off(14) as u16, reg_off(25) as u16, 0, -1);
    // [1] Sub  rd=x22 rs1=x0 rs2=x25
    push(&mut v, 16, reg_off(22) as u16, 16, reg_off(25) as u16, 0);
    // [2] Beq  rs1=x24 rs2=x0  target_pc=0x11fce
    push(&mut v, 32, 0, reg_off(24) as u16, 16, 0x11fce);
    v
}

fn build_and_instantiate() -> (Engine, Module) {
    let engine = Engine::default();
    let (bytes, _used) = codegen::build_block(&ir_block_n555(), 0x11eb0, 0x11ebc, 3, None, None);
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    (engine, module)
}

fn call_with(reg25: i64, reg24: i64) -> (i64, i64 /*x14*/, i64 /*x22*/) {
    let (engine, module) = build_and_instantiate();
    let mut store: Store<()> = Store::new(&engine, ());

    // Give the compiled block a real memory to bind its (import "guest" "mem").
    let mem_ty = MemoryType::new(1, None);
    let mem = Memory::new(&mut store, mem_ty).unwrap();

    // Zero memory, then write reg values.
    let data = mem.data_mut(&mut store);
    for b in data.iter_mut() {
        *b = 0;
    }
    let buf = mem.data_mut(&mut store);
    // x0 stays zero.
    buf[reg_off(24)..reg_off(24) + 8].copy_from_slice(&reg24.to_le_bytes());
    buf[reg_off(25)..reg_off(25) + 8].copy_from_slice(&reg25.to_le_bytes());

    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem)]).unwrap();
    let f = inst
        .get_typed_func::<i32, i64>(&mut store, "block")
        .unwrap();
    let pc_next = f.call(&mut store, 0).unwrap();

    let buf = mem.data(&store);
    let x14 = i64::from_le_bytes(buf[reg_off(14)..reg_off(14) + 8].try_into().unwrap());
    let x22 = i64::from_le_bytes(buf[reg_off(22)..reg_off(22) + 8].try_into().unwrap());
    (pc_next, x14, x22)
}

#[test]
fn branch_taken_when_x24_eq_zero() {
    // x24 == 0 == x0 → branch taken → next_pc = 0x11fce.
    let (pc, x14, x22) = call_with(7, 0);
    assert_eq!(pc, 0x11fce, "branch should be taken (x24 == x0)");
    assert_eq!(x14, 6, "x14 = x25 - 1 = 7 - 1 = 6");
    assert_eq!(x22, -7, "x22 = -x25 = -7");
}

#[test]
fn fallthrough_when_x24_nonzero() {
    // x24 != 0 → branch NOT taken → next_pc = 0x11ebc (block_end_pc).
    let (pc, x14, x22) = call_with(10, 1);
    assert_eq!(pc, 0x11ebc, "branch should fall through (x24 != x0)");
    assert_eq!(x14, 9, "x14 = x25 - 1 = 10 - 1 = 9");
    assert_eq!(x22, -10, "x22 = -x25 = -10");
}

#[test]
fn alu_runs_before_branch_even_when_taken() {
    // Regression: ensure ALU side-effects happen even when branch is taken
    // (they precede the terminator in the IR; must NOT be skipped).
    let (pc, x14, x22) = call_with(-5, 0);
    assert_eq!(pc, 0x11fce);
    assert_eq!(x14, -6);
    assert_eq!(x22, 5);
}
