//! Semantic test of Batch 4 load/store codegen, with the guest helpers
//! replaced by host funcs that record their arguments. Verifies:
//!   - helper-import wiring (only used helpers declared, positional order)
//!   - call argument order/derivation (addr = reg[rs1] + imm, val = reg[rs2],
//!     rd_off passthrough)
//!   - the fault path: nonzero helper return makes the block bail with
//!     (start_pc + pc_off) | 1 and skip all later ops.

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

#[derive(Default)]
struct Calls {
    loads: Vec<(u64, u32)>,
    stores: Vec<(u64, u64)>,
    fail_loads: bool,
}

/// ld x14, 8(x10); sd x12, -16(x11) — no terminator, falls through to end_pc.
fn ir_ld_sd() -> Vec<u8> {
    let mut v = Vec::new();
    // Ld: rd_off, rs1_off = base, rs2_off = pc_off, imm = disp
    push(&mut v, 41, reg_off(14) as u16, reg_off(10) as u16, 4, 8);
    // Sd: rd_off = pc_off, rs1_off = base, rs2_off = src, imm = disp
    push(&mut v, 48, 8, reg_off(11) as u16, reg_off(12) as u16, -16);
    v
}

fn run(start_pc: u64, end_pc: u64, fail_loads: bool, regs: &[(usize, i64)]) -> (i64, Calls) {
    let engine = Engine::default();
    let (bytes, used) = codegen::build_block(&ir_ld_sd(), start_pc, end_pc);
    assert_eq!(used, vec![3, 10], "block should import exactly ld and sd");
    let module = Module::from_binary(&engine, &bytes).expect("module compiles");
    let mut store: Store<Calls> = Store::new(
        &engine,
        Calls {
            fail_loads,
            ..Default::default()
        },
    );
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    for &(n, val) in regs {
        let off = reg_off(n);
        mem.data_mut(&mut store)[off..off + 8].copy_from_slice(&val.to_le_bytes());
    }
    let ld = Func::wrap(
        &mut store,
        |mut c: Caller<'_, Calls>, _state: i32, addr: i64, rd_off: i32| -> i32 {
            c.data_mut().loads.push((addr as u64, rd_off as u32));
            i32::from(c.data().fail_loads)
        },
    );
    let sd = Func::wrap(
        &mut store,
        |mut c: Caller<'_, Calls>, _state: i32, addr: i64, val: i64| -> i32 {
            c.data_mut().stores.push((addr as u64, val as u64));
            0
        },
    );
    let mut externs = vec![Extern::Memory(mem)];
    for &h in &used {
        externs.push(Extern::Func(if h < 7 { ld } else { sd }));
    }
    let inst = Instance::new(&mut store, &module, &externs).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let r = f.call(&mut store, 0).unwrap();
    (r, store.into_data())
}

#[test]
fn load_store_args_and_fallthrough() {
    let (pc, calls) = run(0x1000, 0x100c, false, &[(10, 0x8000), (11, 0x9000), (12, 77)]);
    assert_eq!(pc, 0x100c, "no terminator: falls through to end_pc");
    assert_eq!(calls.loads, vec![(0x8008, reg_off(14) as u32)], "addr = x10 + 8");
    assert_eq!(calls.stores, vec![(0x9000 - 16, 77)], "addr = x11 - 16, val = x12");
}

#[test]
fn load_fault_bails_with_tagged_pc() {
    let (pc, calls) = run(0x1000, 0x100c, true, &[(10, 0x8000), (11, 0x9000), (12, 77)]);
    assert_eq!(
        pc as u64,
        (0x1000 + 4) | 1,
        "fault return = (start_pc + pc_off) | 1"
    );
    assert_eq!(calls.loads.len(), 1);
    assert!(
        calls.stores.is_empty(),
        "ops after the faulting load must not run"
    );
}
