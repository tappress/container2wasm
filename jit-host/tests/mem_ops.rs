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

/// Synthetic TLB layout for the inline-probe tests: 32 entries x 16 bytes,
/// read array at state+0x400, write array at state+0x600 (state_ptr = 0, so
/// these are absolute linear-memory offsets; regs live at 16.. so nothing
/// overlaps).
const TLB: codegen::TlbLayout = codegen::TlbLayout {
    tlb_read_off: 0x400,
    tlb_write_off: 0x600,
    idx_mask: 31,
    entry_shift: 4,
    vaddr_off: 0,
    addend_off: 8,
    pg_shift: 12,
};

/// Install a fake TLB entry: `vaddr_page` tag plus an addend that maps that
/// page's accesses onto `host_base` (addend = host_base - vaddr_page, the
/// same wrapping arithmetic TinyEMU uses).
fn install_entry(mem: &Memory, store: &mut Store<Calls>, array_off: u32, vaddr_page: u64, host_base: u32) {
    let idx = ((vaddr_page >> TLB.pg_shift) & TLB.idx_mask as u64) as u32;
    let e = (array_off + (idx << TLB.entry_shift)) as usize;
    let addend = host_base.wrapping_sub(vaddr_page as u32);
    let data = mem.data_mut(store);
    data[e..e + 8].copy_from_slice(&vaddr_page.to_le_bytes());
    data[e + 8..e + 12].copy_from_slice(&addend.to_le_bytes());
}

fn run(start_pc: u64, end_pc: u64, fail_loads: bool, regs: &[(usize, i64)]) -> (i64, Calls) {
    run_with(start_pc, end_pc, fail_loads, regs, None, |_, _| {}).0
}

fn run_with(
    start_pc: u64,
    end_pc: u64,
    fail_loads: bool,
    regs: &[(usize, i64)],
    tlb: Option<&codegen::TlbLayout>,
    setup: impl FnOnce(&Memory, &mut Store<Calls>),
) -> ((i64, Calls), Vec<u8>) {
    let engine = Engine::default();
    let (bytes, used) = codegen::build_block(&ir_ld_sd(), start_pc, end_pc, tlb);
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
    setup(&mem, &mut store);
    let mut externs = vec![Extern::Memory(mem)];
    for &h in &used {
        externs.push(Extern::Func(if h < 7 { ld } else { sd }));
    }
    let inst = Instance::new(&mut store, &module, &externs).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    let r = f.call(&mut store, 0).unwrap();
    let snapshot = mem.data(&store).to_vec();
    ((r, store.into_data()), snapshot)
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

/// Inline TLB hit: both accesses resolve through the fake TLB entries; the
/// helpers must never be called, the load lands in x14, the store lands at
/// addend-mapped linear memory.
#[test]
fn inline_tlb_hit_skips_helpers() {
    // ld x14, 8(x10): x10 = 0x8000 -> addr 0x8008, page 0x8000.
    // sd x12, -16(x11): x11 = 0x9000 -> addr 0x8ff0, same page 0x8000.
    let regs = [(10, 0x8000i64), (11, 0x9000), (12, 0x1122_3344_5566_7788)];
    let ((pc, calls), snap) = run_with(0x1000, 0x100c, true, &regs, Some(&TLB), |mem, store| {
        install_entry(mem, store, TLB.tlb_read_off, 0x8000, 0x1000); // 0x8008 -> 0x1008
        install_entry(mem, store, TLB.tlb_write_off, 0x8000, 0x2000); // 0x8ff0 -> 0x2ff0
        mem.data_mut(store)[0x1008..0x1010].copy_from_slice(&0xdead_beef_cafe_f00du64.to_le_bytes());
    });
    assert_eq!(pc, 0x100c);
    assert!(calls.loads.is_empty() && calls.stores.is_empty(), "TLB hit must not call helpers");
    let x14 = i64::from_le_bytes(snap[reg_off(14)..reg_off(14) + 8].try_into().unwrap());
    assert_eq!(x14 as u64, 0xdead_beef_cafe_f00d, "load writes back through the addend");
    let stored = u64::from_le_bytes(snap[0x2ff0..0x2ff8].try_into().unwrap());
    assert_eq!(stored, 0x1122_3344_5566_7788, "store goes through the write addend");
}

/// Empty TLB: tag compare fails, both ops fall back to the helper calls with
/// the same args as the helper-only codegen.
#[test]
fn inline_tlb_miss_falls_back_to_helpers() {
    let regs = [(10, 0x8000i64), (11, 0x9000), (12, 77)];
    let ((pc, calls), _) = run_with(0x1000, 0x100c, false, &regs, Some(&TLB), |_, _| {});
    assert_eq!(pc, 0x100c);
    assert_eq!(calls.loads, vec![(0x8008, reg_off(14) as u32)]);
    assert_eq!(calls.stores, vec![(0x9000 - 16, 77)]);
}

/// A valid entry but a misaligned access: the width bits kept by the tag
/// mask make the compare fail, so misalignment never takes the fast path.
#[test]
fn inline_tlb_misaligned_misses() {
    let regs = [(10, 0x8001i64), (11, 0x9001), (12, 77)]; // addrs 0x8009 / 0x8ff1
    let ((pc, calls), _) = run_with(0x1000, 0x100c, false, &regs, Some(&TLB), |mem, store| {
        install_entry(mem, store, TLB.tlb_read_off, 0x8000, 0x1000);
        install_entry(mem, store, TLB.tlb_write_off, 0x8000, 0x2000);
    });
    assert_eq!(pc, 0x100c);
    assert_eq!(calls.loads, vec![(0x8009, reg_off(14) as u32)], "misaligned ld -> helper");
    assert_eq!(calls.stores, vec![(0x8ff1, 77)], "misaligned sd -> helper");
}

/// Miss-path fault: with an empty TLB and a failing load helper, the block
/// must bail with the tagged fault pc, same as the helper-only shape.
#[test]
fn inline_tlb_miss_fault_bails() {
    let regs = [(10, 0x8000i64), (11, 0x9000), (12, 77)];
    let ((pc, calls), _) = run_with(0x1000, 0x100c, true, &regs, Some(&TLB), |_, _| {});
    assert_eq!(pc as u64, (0x1000 + 4) | 1);
    assert_eq!(calls.loads.len(), 1);
    assert!(calls.stores.is_empty());
}
