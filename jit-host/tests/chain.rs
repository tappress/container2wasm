//! Semantic tests of Batch 8 block chaining: the entry self-charge of
//! s->n_cycles, and the exit epilogue that probes a synthetic pc->slot map
//! in linear memory and tail-calls the next block through an imported
//! funcref table. The map-entry layout mirrors jit_interface.h's
//! jit_map_entry; the hash arithmetic must match jit_map_hash/jit_page_hash
//! bit-for-bit or these tests place entries where the epilogue won't look.

use jit_host::codegen;
use wasmtime::*;

const REG_BASE: usize = 16;
fn reg_off(n: usize) -> usize {
    REG_BASE + n * 8
}

const HASH_MULT: u64 = 0x9E3779B97F4A7C15;
const MAP_BITS: u32 = 8; // synthetic: 256 entries keeps the test memory small
const PAGE_GEN_BITS: u32 = 12;
const ENTRY_SIZE: u32 = 40;
const MAP_BASE: u32 = 0x10000;
const USER_GEN_ADDR: u32 = 0x30000;
const GLOBAL_GEN_ADDR: u32 = 0x30004;
const CHAIN_HOPS_ADDR: u32 = 0x30008;
const PAGE_GEN_BASE: u32 = 0x31000;
const N_CYCLES_OFF: u32 = 8; // state+8..12 is free (regs start at 16)

const CHAIN: codegen::ChainLayout = codegen::ChainLayout {
    n_cycles_off: N_CYCLES_OFF,
    map_base: MAP_BASE,
    entry_size: ENTRY_SIZE,
    fn_idx_off: 16,
    user_gen_off: 20,
    global_gen_off: 24,
    page_gen_off: 28,
    map_bits: MAP_BITS,
    page_gen_bits: PAGE_GEN_BITS,
    user_gen_addr: USER_GEN_ADDR,
    global_gen_addr: GLOBAL_GEN_ADDR,
    page_gen_base: PAGE_GEN_BASE,
    chain_hops_addr: CHAIN_HOPS_ADDR,
};

// Live counter values the tests install; entries stamped with these match.
const USER_GEN: u32 = 7;
const GLOBAL_GEN: u32 = 9;
const PAGE_GEN: u32 = 3;

fn map_entry_addr(pc: u64) -> usize {
    let h = ((pc >> 1).wrapping_mul(HASH_MULT) >> (64 - MAP_BITS)) as u32;
    (MAP_BASE + h * ENTRY_SIZE) as usize
}
fn page_gen_addr(pc: u64) -> usize {
    let h = ((pc >> 12).wrapping_mul(HASH_MULT) >> (64 - PAGE_GEN_BITS)) as u32;
    (PAGE_GEN_BASE + h * 4) as usize
}

fn push(v: &mut Vec<u8>, kind: u8, rd: u16, rs1: u16, rs2: u16, imm: i64) {
    v.push(kind);
    v.push(0);
    v.extend_from_slice(&rd.to_le_bytes());
    v.extend_from_slice(&rs1.to_le_bytes());
    v.extend_from_slice(&rs2.to_le_bytes());
    v.extend_from_slice(&imm.to_le_bytes());
}

/// addi xN, xN, K — one-op non-terminator block (falls through to end_pc).
fn ir_addi(reg: usize, k: i64) -> Vec<u8> {
    let mut v = Vec::new();
    push(&mut v, 2, reg_off(reg) as u16, reg_off(reg) as u16, 0, k);
    v
}

struct Harness {
    store: Store<bool>, // data = fail_loads flag for the ld helper
    mem: Memory,
    table: Table,
    engine: Engine,
}

impl Harness {
    fn new() -> Self {
        let engine = Engine::default();
        let mut store: Store<bool> = Store::new(&engine, false);
        let mem = Memory::new(&mut store, MemoryType::new(4, None)).unwrap();
        let table = Table::new(
            &mut store,
            TableType::new(RefType::FUNCREF, 1, None), // slot 0 = null, like wasm-ld
            Ref::Func(None),
        )
        .unwrap();
        let mut h = Harness { store, mem, table, engine };
        h.w32(USER_GEN_ADDR as usize, USER_GEN);
        h.w32(GLOBAL_GEN_ADDR as usize, GLOBAL_GEN);
        h
    }
    fn w32(&mut self, addr: usize, v: u32) {
        self.mem.data_mut(&mut self.store)[addr..addr + 4].copy_from_slice(&v.to_le_bytes());
    }
    fn w64(&mut self, addr: usize, v: u64) {
        self.mem.data_mut(&mut self.store)[addr..addr + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn r64(&self, addr: usize) -> u64 {
        u64::from_le_bytes(self.mem.data(&self.store)[addr..addr + 8].try_into().unwrap())
    }
    fn set_n_cycles(&mut self, v: i32) {
        self.w32(N_CYCLES_OFF as usize, v as u32);
    }
    fn n_cycles(&self) -> i32 {
        i32::from_le_bytes(
            self.mem.data(&self.store)[N_CYCLES_OFF as usize..N_CYCLES_OFF as usize + 4]
                .try_into()
                .unwrap(),
        )
    }
    fn chain_hops(&self) -> u64 {
        self.r64(CHAIN_HOPS_ADDR as usize)
    }

    /// Build + instantiate a chain-mode block; returns its callable func.
    fn block(&mut self, ir: &[u8], start_pc: u64, end_pc: u64, n_insns: u32) -> TypedFunc<i32, i64> {
        let (bytes, used) =
            codegen::build_block(ir, start_pc, end_pc, n_insns, None, Some(&CHAIN), false);
        let module = Module::from_binary(&self.engine, &bytes).expect("module compiles");
        let mut externs = vec![Extern::Memory(self.mem), Extern::Table(self.table)];
        let fail_ld = Func::wrap(
            &mut self.store,
            |c: Caller<'_, bool>, _state: i32, _addr: i64, _rd_off: i32| -> i32 {
                i32::from(*c.data())
            },
        );
        for _ in &used {
            externs.push(Extern::Func(fail_ld)); // tests only ever use loads
        }
        let inst = Instance::new(&mut self.store, &module, &externs).unwrap();
        inst.get_typed_func::<i32, i64>(&mut self.store, "block").unwrap()
    }

    /// Register a block in the synthetic map: grow the table, write the
    /// entry with the given gen stamps, install the matching page gen.
    fn map_insert(&mut self, pc: u64, f: &TypedFunc<i32, i64>, user_gen: u32, global_gen: u32) {
        let slot = self
            .table
            .grow(&mut self.store, 1, Ref::Func(Some(*f.func())))
            .unwrap() as u32;
        self.w32(page_gen_addr(pc), PAGE_GEN);
        let e = map_entry_addr(pc);
        self.w64(e, pc);
        self.w32(e + 16, slot);
        self.w32(e + 20, user_gen);
        self.w32(e + 24, global_gen);
        self.w32(e + 28, PAGE_GEN);
    }
}

/// A (addi x5 += 1) falls through to 0x1008 where B (addi x6 += 2) is
/// registered; B's own fallthrough 0x1010 has no entry. One call to A must
/// run both blocks via a tail-call hop and return B's next_pc.
#[test]
fn chain_hop_executes_target() {
    let mut h = Harness::new();
    let b = h.block(&ir_addi(6, 2), 0x1008, 0x1010, 1);
    h.map_insert(0x1008, &b, USER_GEN, GLOBAL_GEN);
    let a = h.block(&ir_addi(5, 1), 0x1000, 0x1008, 2);
    h.set_n_cycles(1000);
    let r = a.call(&mut h.store, 0).unwrap();
    assert_eq!(r, 0x1010, "returns B's fallthrough after the hop");
    assert_eq!(h.r64(reg_off(5)), 1, "A executed");
    assert_eq!(h.r64(reg_off(6)), 2, "B executed via the chained tail-call");
    assert_eq!(h.chain_hops(), 1);
    assert_eq!(h.n_cycles(), 1000 - 2 - 1, "both blocks self-charged exactly");
}

/// Self-charge drives n_cycles to <= 0: the epilogue must return to the
/// dispatch loop (so interrupts get serviced) instead of hopping.
#[test]
fn budget_exhaustion_returns_without_hop() {
    let mut h = Harness::new();
    let b = h.block(&ir_addi(6, 2), 0x1008, 0x1010, 1);
    h.map_insert(0x1008, &b, USER_GEN, GLOBAL_GEN);
    let a = h.block(&ir_addi(5, 1), 0x1000, 0x1008, 2);
    h.set_n_cycles(1); // A charges 2 -> -1
    let r = a.call(&mut h.store, 0).unwrap();
    assert_eq!(r, 0x1008, "returns A's own next_pc");
    assert_eq!(h.r64(reg_off(6)), 0, "B must not run");
    assert_eq!(h.chain_hops(), 0);
    assert_eq!(h.n_cycles(), -1, "self-charge still applied");
}

/// Stale generation stamps (user / global / page, each in turn) must fail
/// the probe and fall back to the dispatch loop.
#[test]
fn stale_gens_fall_back() {
    for which in ["user", "global", "page"] {
        let mut h = Harness::new();
        let b = h.block(&ir_addi(6, 2), 0x1008, 0x1010, 1);
        let (ug, gg) = match which {
            "user" => (USER_GEN - 1, GLOBAL_GEN),
            "global" => (USER_GEN, GLOBAL_GEN - 1),
            _ => (USER_GEN, GLOBAL_GEN),
        };
        h.map_insert(0x1008, &b, ug, gg);
        if which == "page" {
            h.w32(page_gen_addr(0x1008), PAGE_GEN + 1); // counter moved on
        }
        let a = h.block(&ir_addi(5, 1), 0x1000, 0x1008, 2);
        h.set_n_cycles(1000);
        let r = a.call(&mut h.store, 0).unwrap();
        assert_eq!(r, 0x1008, "stale {which} gen: no hop");
        assert_eq!(h.r64(reg_off(6)), 0, "stale {which} gen: B must not run");
        assert_eq!(h.chain_hops(), 0);
    }
}

/// Kernel-half pcs skip the user_gen check (globally mapped, survive satp
/// rolls) — a stale user stamp must still chain there.
#[test]
fn kernel_half_skips_user_gen() {
    const KPC: u64 = 0xffffffc000001008;
    let mut h = Harness::new();
    let b = h.block(&ir_addi(6, 2), KPC, KPC + 8, 1);
    h.map_insert(KPC, &b, USER_GEN - 1, GLOBAL_GEN); // stale user gen
    let a = h.block(&ir_addi(5, 1), KPC - 8, KPC, 2);
    h.set_n_cycles(1000);
    let r = a.call(&mut h.store, 0).unwrap();
    assert_eq!(r as u64, KPC + 8, "kernel-half target chains despite user gen");
    assert_eq!(h.r64(reg_off(6)), 2);
    assert_eq!(h.chain_hops(), 1);
}

/// An MMU-fault bail must return the tagged pc directly — never probe the
/// map — but the entry self-charge still applies (conservative overcharge,
/// same as the C hook). IR: ld x14, 0(x10) with a failing helper.
#[test]
fn fault_bails_without_chain() {
    let mut h = Harness::new();
    let mut ir = Vec::new();
    push(&mut ir, 41, reg_off(14) as u16, reg_off(10) as u16, 0, 0); // Ld, pc_off 0
    let blk = h.block(&ir, 0x2000, 0x2004, 1);
    // Even a valid entry for the fault pc must not be taken.
    let b = h.block(&ir_addi(6, 2), 0x2001, 0x2009, 1);
    h.map_insert(0x2001, &b, USER_GEN, GLOBAL_GEN);
    h.set_n_cycles(1000);
    *h.store.data_mut() = true; // fail loads
    let r = blk.call(&mut h.store, 0).unwrap();
    assert_eq!(r as u64, 0x2000 | 1, "tagged fault pc");
    assert_eq!(h.r64(reg_off(6)), 0, "no chain on the fault path");
    assert_eq!(h.chain_hops(), 0);
    assert_eq!(h.n_cycles(), 999, "entry self-charge applied before the fault");
}

/// Sanity: with chain disabled (None), the same IR produces the legacy
/// shape — no table import, no self-charge.
#[test]
fn no_chain_mode_is_legacy_shape() {
    let engine = Engine::default();
    let (bytes, _) = codegen::build_block(&ir_addi(5, 1), 0x1000, 0x1008, 2, None, None, false);
    let module = Module::from_binary(&engine, &bytes).unwrap();
    let n_tables = module.imports().filter(|i| matches!(i.ty(), ExternType::Table(_))).count();
    assert_eq!(n_tables, 0, "legacy blocks must not import a table");
    let mut store: Store<()> = Store::new(&engine, ());
    let mem = Memory::new(&mut store, MemoryType::new(1, None)).unwrap();
    let inst = Instance::new(&mut store, &module, &[Extern::Memory(mem)]).unwrap();
    let f = inst.get_typed_func::<i32, i64>(&mut store, "block").unwrap();
    assert_eq!(f.call(&mut store, 0).unwrap(), 0x1008);
    let nc = i32::from_le_bytes(mem.data(&store)[8..12].try_into().unwrap());
    assert_eq!(nc, 0, "legacy blocks must not touch n_cycles");
}
