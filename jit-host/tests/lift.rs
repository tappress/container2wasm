//! Semantic tests of Batch 6 register lifting (`lift_flag = true` in
//! build_block). Each builds a real block, runs it under wasmtime, and checks
//! the guest register file in memory — the only place correctness is visible,
//! since lifting must be transparent to everything outside the block.
//!
//! Coverage:
//!   - transparency: a lifted block produces byte-identical register state to
//!     the same block compiled without lifting (differential).
//!   - intra-block value flow: a register written, re-read, and written again
//!     threads through its local and spills the final value.
//!   - fault spill (the load-bearing invariant): an MMU-fault bail commits the
//!     registers earlier ops wrote, so the interpreter re-executes correctly.
//!   - helper-call survival: a lifted register held across a successful helper
//!     load stays live in its local, while the load's own dest (helper-written,
//!     never lifted) is read back from memory.

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

const K_ADDI: u8 = 2;
const K_ADD: u8 = 15;
const K_LD: u8 = 41;

fn read_reg(snap: &[u8], n: usize) -> i64 {
    i64::from_le_bytes(snap[reg_off(n)..reg_off(n) + 8].try_into().unwrap())
}

/// Run a no-helper (pure ALU) block, returning (next_pc, memory snapshot).
fn run_alu(ir: &[u8], start_pc: u64, end_pc: u64, n_insns: u32, lift: bool, regs: &[(usize, i64)]) -> (i64, Vec<u8>) {
    let engine = Engine::default();
    let (bytes, used) = codegen::build_block(ir, start_pc, end_pc, n_insns, None, None, lift);
    assert!(used.is_empty(), "pure ALU block imports no helpers");
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    let mut store: Store<()> = Store::new(&engine, ());
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    for &(n, v) in regs {
        mem.data_mut(&mut store)[reg_off(n)..reg_off(n) + 8].copy_from_slice(&v.to_le_bytes());
    }
    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem)]).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let pc = f.call(&mut store, 0).unwrap();
    (pc, mem.data(&store).to_vec())
}

/// addi x5,x5,1 ; add x6,x5,x5 ; addi x5,x5,10  (no terminator).
/// Exercises a register that is read, written, re-read, and re-written within
/// one block — the case lifting must thread through a local.
fn ir_value_flow() -> Vec<u8> {
    let mut v = Vec::new();
    push(&mut v, K_ADDI, reg_off(5) as u16, reg_off(5) as u16, 0, 1);
    push(&mut v, K_ADD, reg_off(6) as u16, reg_off(5) as u16, reg_off(5) as u16, 0);
    push(&mut v, K_ADDI, reg_off(5) as u16, reg_off(5) as u16, 0, 10);
    v
}

#[test]
fn lift_intra_block_value_flow() {
    let (pc, snap) = run_alu(&ir_value_flow(), 0x1000, 0x100c, 3, true, &[(5, 100)]);
    assert_eq!(pc, 0x100c, "non-terminator falls through to end_pc");
    // op1: x5 = 101; op2: x6 = 202; op3: x5 = 111.
    assert_eq!(read_reg(&snap, 5), 111, "x5 threads 100 -> 101 -> 111 through its local");
    assert_eq!(read_reg(&snap, 6), 202, "x6 = 2 * (intermediate x5 = 101)");
}

#[test]
fn lift_matches_no_lift() {
    // Differential transparency over a mix of values, incl. a negative.
    for &x5 in &[0i64, 1, 100, -7, i64::from(i32::MIN)] {
        let (pc_on, snap_on) = run_alu(&ir_value_flow(), 0x1000, 0x100c, 3, true, &[(5, x5)]);
        let (pc_off, snap_off) = run_alu(&ir_value_flow(), 0x1000, 0x100c, 3, false, &[(5, x5)]);
        assert_eq!(pc_on, pc_off, "next_pc identical for x5={x5}");
        assert_eq!(read_reg(&snap_on, 5), read_reg(&snap_off, 5), "x5 identical for x5={x5}");
        assert_eq!(read_reg(&snap_on, 6), read_reg(&snap_off, 6), "x6 identical for x5={x5}");
    }
}

/// State for the helper-backed lift tests: the ld helper either faults or
/// writes `load_value` into reg[rd], using the stashed Memory handle.
#[derive(Default)]
struct LdState {
    mem: Option<Memory>,
    load_value: i64,
    fail: bool,
    calls: u32,
}

/// Build, instantiate, and run a block whose only helper is `ld` (id 3),
/// wired to the LdState behavior. Returns (next_pc, memory snapshot).
fn run_with_ld(ir: &[u8], start_pc: u64, end_pc: u64, n_insns: u32, regs: &[(usize, i64)], fail: bool, load_value: i64) -> (i64, Vec<u8>, u32) {
    let engine = Engine::default();
    let (bytes, used) = codegen::build_block(ir, start_pc, end_pc, n_insns, None, None, true);
    assert_eq!(used, vec![3], "block imports exactly the ld helper");
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    let mut store: Store<LdState> = Store::new(&engine, LdState { fail, load_value, ..Default::default() });
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    store.data_mut().mem = Some(mem);
    for &(n, v) in regs {
        mem.data_mut(&mut store)[reg_off(n)..reg_off(n) + 8].copy_from_slice(&v.to_le_bytes());
    }
    let ld = Func::wrap(
        &mut store,
        |mut c: Caller<'_, LdState>, state: i32, _addr: i64, rd_off: i32| -> i32 {
            c.data_mut().calls += 1;
            if c.data().fail {
                return 1; // MMU fault: no writeback
            }
            if rd_off != 0 {
                let mem = c.data().mem.unwrap();
                let val = c.data().load_value;
                let off = state as usize + rd_off as usize;
                mem.data_mut(&mut c)[off..off + 8].copy_from_slice(&val.to_le_bytes());
            }
            0
        },
    );
    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem), Extern::Func(ld)]).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let pc = f.call(&mut store, 0).unwrap();
    let calls = store.data().calls;
    (pc, mem.data(&store).to_vec(), calls)
}

/// addi x5,x5,100 ; ld x6,0(x7) [FAULT] ; addi x5,x5,1
/// The faulting load must bail AFTER spilling the first addi's result, and the
/// trailing addi must not run.
fn ir_fault_mid_block() -> Vec<u8> {
    let mut v = Vec::new();
    push(&mut v, K_ADDI, reg_off(5) as u16, reg_off(5) as u16, 0, 100);
    // ld x6, 0(x7): rd=x6, rs1=x7 base, rs2=pc_off (4), imm=0
    push(&mut v, K_LD, reg_off(6) as u16, reg_off(7) as u16, 4, 0);
    push(&mut v, K_ADDI, reg_off(5) as u16, reg_off(5) as u16, 0, 1);
    v
}

#[test]
fn lift_fault_spills_earlier_writes() {
    let (pc, snap, calls) = run_with_ld(&ir_fault_mid_block(), 0x2000, 0x200c, 3, &[(5, 7), (6, 999), (7, 0x8000)], true, 0);
    assert_eq!(pc as u64, (0x2000 + 4) | 1, "fault return = (start_pc + pc_off) | 1");
    assert_eq!(calls, 1, "the faulting load is reached once");
    // The load-bearing assertion: the first addi's write reached memory before
    // the bail, so the interpreter re-executing from the fault sees it.
    assert_eq!(read_reg(&snap, 5), 107, "x5 (+100) spilled before the fault bail");
    assert_eq!(read_reg(&snap, 6), 999, "faulting load performs no writeback");
    assert_ne!(read_reg(&snap, 5), 108, "the addi after the fault must not run");
}

/// addi x5,x5,7 ; ld x6,0(x7) [writes 50] ; add x5,x5,x6
/// x5 is lifted and must survive the helper call; x6 is helper-written (never
/// lifted) and must be read back from memory after the helper writes it.
fn ir_reg_survives_helper() -> Vec<u8> {
    let mut v = Vec::new();
    push(&mut v, K_ADDI, reg_off(5) as u16, reg_off(5) as u16, 0, 7);
    push(&mut v, K_LD, reg_off(6) as u16, reg_off(7) as u16, 4, 0);
    push(&mut v, K_ADD, reg_off(5) as u16, reg_off(5) as u16, reg_off(6) as u16, 0);
    v
}

#[test]
fn lift_reg_survives_helper_call() {
    let (pc, snap, calls) = run_with_ld(&ir_reg_survives_helper(), 0x3000, 0x300c, 3, &[(5, 1000), (6, -1), (7, 0x8000)], false, 50);
    assert_eq!(pc, 0x300c, "non-terminator falls through");
    assert_eq!(calls, 1);
    assert_eq!(read_reg(&snap, 6), 50, "helper wrote x6 in memory");
    assert_eq!(read_reg(&snap, 5), 1057, "x5 local (1000+7) survives the call, then += loaded x6 (50)");
}
