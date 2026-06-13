//! IR -> wasm codegen for compiled basic blocks.
//!
//! A compiled BB has signature `(param state_ptr i32) (result next_pc i64)`.
//! It imports the guest's linear memory as `(import "guest" "mem" (memory N))`
//! and reads/writes guest registers through `i64.load/store offset=...` against
//! `state_ptr` (i.e. `&RISCVCPUState->reg[N]`).
//!
//! Returning `next_pc = 0` signals "fall through to the interpreter" (used for
//! any block exit we don't yet codegen, e.g. branches in Batch 2). Otherwise
//! the returned PC is what the interpreter uses for the next dispatch.
//!
//! IR is a flat byte stream from the C scanner. See [crate::ir].

use crate::ir::{LoadW, MdOp, Op, StoreW, parse_ir};
use wasm_encoder::*;

/// Guest export names of the memory helpers, indexed by helper id (loads
/// 0-6 = lb/lh/lw/ld/lbu/lhu/lwu, stores 7-10 = sb/sh/sw/sd, atomics
/// 11-12 = amo_w/amo_d). main.rs resolves these from the c2w instance and
/// passes them as block imports positionally, so the order here is
/// load-bearing. The atomics pair is optional (absent on pre-Batch-4.5
/// guests); blocks using them then fail registration and stay interpreted.
pub const MEM_HELPER_EXPORTS: [&str; 13] = [
    "c2w_jit_lb",
    "c2w_jit_lh",
    "c2w_jit_lw",
    "c2w_jit_ld",
    "c2w_jit_lbu",
    "c2w_jit_lhu",
    "c2w_jit_lwu",
    "c2w_jit_sb",
    "c2w_jit_sh",
    "c2w_jit_sw",
    "c2w_jit_sd",
    "c2w_jit_amo_w",
    "c2w_jit_amo_d",
];

/// Helper ids 0..BASE_HELPERS-1 must all be present for any mem codegen;
/// ids from BASE_HELPERS up are optional extensions.
pub const BASE_HELPERS: usize = 11;

/// TLB layout constants read once from the guest's `c2w_jit_tlb_layout`
/// export (Batch 5). All offsets are byte offsets in the guest's linear
/// memory: tlb_read_off/tlb_write_off relative to the state pointer,
/// vaddr_off/addend_off relative to a TLBEntry base. Present = memory ops
/// compile to an inline probe of the live TLB with the helper call demoted
/// to the miss path; absent = helper-call-only shape, the Batch 4 codegen.
///
/// OPT-IN ONLY (JIT_INLINE_TLB=1): measured a net LOSS under wasmtime —
/// loop 70.9s inline vs 68.5s helper-only, same artifact, same day. The
/// probe hits (the helpers' identical C probe demonstrably hits), but the
/// cross-module call it eliminates costs only a few ns while the inlined
/// probe adds ~30 wasm ops per mem op across thousands of blocks (icache +
/// compile time) versus one shared clang-optimized helper. Kept for the
/// browser port, where engine call costs may price this differently.
#[derive(Clone, Copy, Debug)]
pub struct TlbLayout {
    pub tlb_read_off: u32,
    pub tlb_write_off: u32,
    /// TLB_SIZE - 1 (TLB_SIZE asserted power of two by the caller).
    pub idx_mask: u32,
    /// log2(sizeof(TLBEntry)).
    pub entry_shift: u32,
    pub vaddr_off: u32,
    pub addend_off: u32,
    pub pg_shift: u32,
}

/// Chain layout constants read once from the guest's `c2w_jit_tlb_layout`
/// export (Batch 8, selectors 7-19), used only when JIT_CHAIN=1 opts in
/// (measured a net loss on boot-heavy workloads under wasmtime — see the
/// main.rs comment at the query site). Present = every block gets (a) an entry
/// prologue that decrements `s->n_cycles` by its own insn count ("self-
/// charge" — the host must ack this to the guest via selector 20 so the C
/// dispatch hook stops charging), and (b) an exit epilogue that probes the
/// guest's own pc->slot map — replicating jit_map_lookup's pc + generation
/// checks bit-for-bit — and `return_call_indirect`s straight into the next
/// compiled block while cycle budget remains. The C dispatch loop is then
/// only re-entered on a miss, a stale generation, an MMU-fault bail, or
/// budget exhaustion (which is what lets timer interrupts in: `mip` can't
/// newly assert mid-chain because CSR writes and MMIO are scan terminators /
/// helper slow paths, so honoring `n_cycles` preserves the interpreter's
/// interrupt latency exactly).
///
/// All `*_addr` / `*_base` fields are absolute wasm32 linear-memory addresses
/// of guest globals; offsets are field offsets within their structs.
#[derive(Clone, Copy, Debug)]
pub struct ChainLayout {
    pub n_cycles_off: u32,
    pub map_base: u32,
    pub entry_size: u32,
    pub fn_idx_off: u32,
    pub user_gen_off: u32,
    pub global_gen_off: u32,
    pub page_gen_off: u32,
    pub map_bits: u32,
    pub page_gen_bits: u32,
    pub user_gen_addr: u32,
    pub global_gen_addr: u32,
    pub page_gen_base: u32,
    pub chain_hops_addr: u32,
}

/// Spec constants of the guest's map design, duplicated from jit_interface.h
/// (jit_map_hash / jit_page_hash / JIT_KERNEL_VA_BASE). The hash multiplier
/// and page shift are baked into both the C lookup and the emitted epilogue;
/// they can only change together.
const HASH_MULT: i64 = 0x9E3779B97F4A7C15u64 as i64;
const PAGE_SHIFT: i64 = 12;
const KERNEL_VA_BASE: i64 = 0xffffffc000000000u64 as i64;

/// The fast-path tag the C macros compare against:
/// `addr & ~(PG_MASK & ~(bytes - 1))`. Page bits select the entry; the kept
/// low bits make any access misaligned for its width fail the compare (TLB
/// vaddrs are page-aligned), so the fast path only ever sees naturally
/// aligned RAM accesses.
fn tag_mask(pg_shift: u32, bytes: u64) -> i64 {
    let pg_mask = (1u64 << pg_shift) - 1;
    !(pg_mask & !(bytes - 1)) as i64
}

/// Helper-import identity of a memory op, or None for non-memory ops.
fn helper_id(op: &Op) -> Option<usize> {
    match *op {
        Op::Load { w, .. } => Some(match w {
            LoadW::B => 0,
            LoadW::H => 1,
            LoadW::W => 2,
            LoadW::D => 3,
            LoadW::Bu => 4,
            LoadW::Hu => 5,
            LoadW::Wu => 6,
        }),
        Op::Store { w, .. } => Some(match w {
            StoreW::B => 7,
            StoreW::H => 8,
            StoreW::W => 9,
            StoreW::D => 10,
        }),
        Op::Amo { d, .. } => Some(if d { 12 } else { 11 }),
        _ => None,
    }
}

/// Register-lifting plan for one block (Batch 6). Guest registers live at fixed
/// byte offsets in the guest's linear memory (offset = offsetof(reg[0]) +
/// n*8); the naive codegen re-loads and re-stores them around every op. Lifting
/// keeps a register in a wasm local for the block's lifetime so within-block
/// reads/writes hit the local.
///
/// Offset 0 is special: it is the x0-discard sentinel the scanner emits for
/// loads/AMO whose dest is x0 (real registers start at offsetof(reg[0]) != 0),
/// and `op_lift_write`/`op_helper_write` already guard `rd != 0`, so it never
/// enters a lift set. x0 as a *source* reads as 0 from memory and is never
/// written, so whether it is lifted (base != 0) or skipped (base == 0), the
/// result is correct and carries no spill.
///
/// Three correctness invariants, all enforced by `analyze_lift`:
///   1. A register written by a memory/AMO **helper** (load.rd, amo.rd) is
///      never lifted — the C helper writes guest memory directly, so a lifted
///      local would diverge across the call. Those stay fully memory-backed.
///   2. Every lifted register is spilled back to memory at EVERY block exit:
///      the normal end, the chain hand-off, and the mid-block MMU-fault bail
///      (the interpreter re-executes the faulting insn and must observe the
///      register values that earlier ops in this block wrote).
///   3. Any register that might be spilled at a fault bail is loaded at entry,
///      so its local holds the live memory value even when its own write comes
///      after the fault point (spilling it back is then a no-op rather than a
///      clobber). Fault-free blocks skip that safety load — only read-before-
///      write registers are loaded at entry there, keeping hot ALU loops tight.
struct Lift {
    /// (reg byte offset, wasm local index, written → must spill at exit).
    regs: Vec<(u32, u32, bool)>,
    /// (offset, local) pairs to initialize from guest memory at block entry.
    entry: Vec<(u32, u32)>,
}

impl Lift {
    fn empty() -> Self {
        Lift { regs: Vec::new(), entry: Vec::new() }
    }
    fn local_of(&self, off: u32) -> Option<u32> {
        self.regs.iter().find(|(o, _, _)| *o == off).map(|&(_, l, _)| l)
    }
}

/// Source register offsets an op reads (real registers only — never the pc_off
/// that loads/stores pack into a spare reg field). x0 (offset 0) may appear and
/// is filtered by the caller.
fn op_reads(op: &Op) -> ([u32; 2], usize) {
    use Op::*;
    match *op {
        Const { .. } | Jal { .. } => ([0, 0], 0),
        Addi { rs1, .. } | Addiw { rs1, .. } | Andi { rs1, .. } | Ori { rs1, .. }
        | Xori { rs1, .. } | Slti { rs1, .. } | Sltiu { rs1, .. } | Slli { rs1, .. }
        | Srli { rs1, .. } | Srai { rs1, .. } | Slliw { rs1, .. } | Srliw { rs1, .. }
        | Sraiw { rs1, .. } | Jalr { rs1, .. } | Load { rs1, .. } => ([rs1, 0], 1),
        Add { rs1, rs2, .. } | Sub { rs1, rs2, .. } | And { rs1, rs2, .. }
        | Or { rs1, rs2, .. } | Xor { rs1, rs2, .. } | Sll { rs1, rs2, .. }
        | Srl { rs1, rs2, .. } | Sra { rs1, rs2, .. } | Slt { rs1, rs2, .. }
        | Sltu { rs1, rs2, .. } | Addw { rs1, rs2, .. } | Subw { rs1, rs2, .. }
        | Sllw { rs1, rs2, .. } | Srlw { rs1, rs2, .. } | Sraw { rs1, rs2, .. }
        | Beq { rs1, rs2, .. } | Bne { rs1, rs2, .. } | Blt { rs1, rs2, .. }
        | Bge { rs1, rs2, .. } | Bltu { rs1, rs2, .. } | Bgeu { rs1, rs2, .. }
        | Store { rs1, rs2, .. } | MulDiv { rs1, rs2, .. } | Amo { rs1, rs2, .. } => {
            ([rs1, rs2], 2)
        }
    }
}

/// The register an op writes via inline codegen (kept in a local when lifted).
/// None for memory/AMO writebacks (a helper writes those — see
/// `op_helper_write`), stores/branches (no register dest), and x0.
fn op_lift_write(op: &Op) -> Option<u32> {
    use Op::*;
    let rd = match *op {
        Const { rd, .. } | Addi { rd, .. } | Addiw { rd, .. } | Andi { rd, .. }
        | Ori { rd, .. } | Xori { rd, .. } | Slti { rd, .. } | Sltiu { rd, .. }
        | Slli { rd, .. } | Srli { rd, .. } | Srai { rd, .. } | Slliw { rd, .. }
        | Srliw { rd, .. } | Sraiw { rd, .. } | Add { rd, .. } | Sub { rd, .. }
        | And { rd, .. } | Or { rd, .. } | Xor { rd, .. } | Sll { rd, .. }
        | Srl { rd, .. } | Sra { rd, .. } | Slt { rd, .. } | Sltu { rd, .. }
        | Addw { rd, .. } | Subw { rd, .. } | Sllw { rd, .. } | Srlw { rd, .. }
        | Sraw { rd, .. } | MulDiv { rd, .. } | Jal { rd, .. } | Jalr { rd, .. } => rd,
        Load { .. } | Store { .. } | Amo { .. } | Beq { .. } | Bne { .. } | Blt { .. }
        | Bge { .. } | Bltu { .. } | Bgeu { .. } => return None,
    };
    if rd != 0 { Some(rd) } else { None }
}

/// The register a C helper writes directly in guest memory (load.rd, amo.rd,
/// nonzero). These are excluded from lifting so a local can't diverge from the
/// memory the helper updates.
fn op_helper_write(op: &Op) -> Option<u32> {
    match *op {
        Op::Load { rd, .. } | Op::Amo { rd, .. } if rd != 0 => Some(rd),
        _ => None,
    }
}

/// Build the register-lifting plan: which registers get a local, which must be
/// loaded at entry, which spilled at exit. `base` is the first local index free
/// after scratch (and chain) locals. See [Lift] for the correctness rules.
fn analyze_lift(ops: &[Op], base: u32) -> Lift {
    // Registers a helper writes in memory — never lifted.
    let mut helper_written: Vec<u32> = Vec::new();
    for op in ops {
        if let Some(o) = op_helper_write(op) {
            if !helper_written.contains(&o) {
                helper_written.push(o);
            }
        }
    }
    // A fault can only occur at a memory/AMO op; their presence forces the
    // safety entry-load of every written register (invariant 3).
    let has_fault = ops
        .iter()
        .any(|op| matches!(op, Op::Load { .. } | Op::Store { .. } | Op::Amo { .. }));

    // Walk in order, recording first-appearance kind (read vs write) and
    // whether the register is ever written by inline code. Reads are processed
    // before the write within an op, matching execution order (e.g. add rd,rd,x
    // counts rd as read-before-write).
    // (off, first_is_read, written)
    let mut infos: Vec<(u32, bool, bool)> = Vec::new();
    for op in ops {
        let (reads, n) = op_reads(op);
        for &r in &reads[..n] {
            if r == 0 || helper_written.contains(&r) {
                continue; // x0 / helper-backed: not lifted
            }
            if !infos.iter().any(|(o, _, _)| *o == r) {
                infos.push((r, true, false));
            }
        }
        if let Some(w) = op_lift_write(op) {
            if w == 0 || helper_written.contains(&w) {
                continue;
            }
            if let Some(info) = infos.iter_mut().find(|(o, _, _)| *o == w) {
                info.2 = true;
            } else {
                infos.push((w, false, true));
            }
        }
    }

    let mut regs = Vec::with_capacity(infos.len());
    let mut entry = Vec::new();
    for (i, &(off, first_read, written)) in infos.iter().enumerate() {
        let local = base + i as u32;
        regs.push((off, local, written));
        if first_read || (written && has_fault) {
            entry.push((off, local));
        }
    }
    Lift { regs, entry }
}

/// Emit the entry prologue that loads lifted registers from guest memory into
/// their locals. Stack-neutral.
fn emit_entry_loads(f: &mut Function, lift: &Lift) {
    for &(off, local) in &lift.entry {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Load(memarg64(off)));
        f.instruction(&Instruction::LocalSet(local));
    }
}

/// Spill every written lifted register back to guest memory. Stack-neutral, so
/// it can run with a terminator's next_pc already on the stack. Emitted at the
/// normal block end and at the mid-block fault bail.
fn spill(f: &mut Function, ctx: &Ctx) {
    for &(off, local, written) in &ctx.lift.regs {
        if written {
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::LocalGet(local));
            f.instruction(&Instruction::I64Store(memarg64(off)));
        }
    }
}

/// Per-block codegen context: constants baked into emitted code plus the
/// mapping from helper id to this module's function-import index.
struct Ctx {
    start_pc: u64,
    block_end_pc: u64,
    helper_import_idx: [Option<u32>; 13],
    tlb: Option<TlbLayout>,
    lift: Lift,
}

/// Locals used by the chain epilogue, appended after the existing scratch
/// groups (1,2 i64; 3 i32; 4-6 i64).
const CHAIN_NEXT: u32 = 7; // i64: the block's computed next_pc
const CHAIN_EPTR: u32 = 8; // i32: candidate jit_map_entry address

/// Build a wasm module containing one exported function "block" with signature
/// `(state_ptr: i32) -> (next_pc: i64)` implementing the given IR sequence.
///
/// `start_pc` is the block's first guest PC — memory ops bake
/// `start_pc + pc_off` as their tagged fault-return value, so the content
/// cache must key on it. `block_end_pc` is the absolute guest PC at which the
/// compiled run ends.
///
/// Returns the module bytes plus the helper ids the block imports, in import
/// order — the caller must pass exactly those guest funcs (after the memory,
/// and after the funcref table when `chain` is set) to instantiation.
///
/// `n_insns` is the guest instruction count the scanner measured for this
/// block — NOT derivable from the IR (x0-write nops emit no IR tuple) — and
/// is baked as the self-charge constant when chaining. Ignored otherwise.
pub fn build_block(
    ir: &[u8],
    start_pc: u64,
    block_end_pc: u64,
    n_insns: u32,
    tlb: Option<&TlbLayout>,
    chain: Option<&ChainLayout>,
    lift_flag: bool,
) -> (Vec<u8>, Vec<usize>) {
    let ops = parse_ir(ir);

    // Which memory helpers does this block use? Imports are declared only
    // for those (ascending helper id), so pure ALU/branch blocks keep the
    // old single-memory-import shape and work against guests that don't
    // export the helpers.
    let mut used: Vec<usize> = ops.iter().filter_map(helper_id).collect();
    used.sort_unstable();
    used.dedup();
    let mut helper_import_idx = [None; 13];
    for (i, &h) in used.iter().enumerate() {
        helper_import_idx[h] = Some(i as u32);
    }

    let mut module = Module::new();

    // Types: 0 = block (i32)->(i64), 1 = load helper (state, addr, rd_off)
    // -> fault, 2 = store helper (state, addr, val) -> fault, 3 = amo helper
    // (state, addr, val2, funct5, rd_off) -> fault. All emitted
    // unconditionally so indices stay fixed.
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I64]);
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64, ValType::I32], vec![ValType::I32]);
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64, ValType::I64], vec![ValType::I32]);
    types.ty().function(
        vec![ValType::I32, ValType::I64, ValType::I64, ValType::I32, ValType::I32],
        vec![ValType::I32],
    );
    module.section(&types);

    // Imports: guest.mem (we don't actually need any min pages; the host
    // overrides with the real guest memory at instantiate time), then the
    // used memory helpers as function imports 0..n-1.
    let mut imports = ImportSection::new();
    imports.import(
        "guest",
        "mem",
        EntityType::Memory(MemoryType {
            minimum: 0,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        }),
    );
    // Chain mode: import the guest module's own funcref table (the one its
    // pc->slot map indexes and the host grows at registration) so the
    // epilogue can return_call_indirect into the next block. Table imports
    // don't consume function-index space, so helper indices are unaffected.
    if chain.is_some() {
        imports.import(
            "guest",
            "table",
            EntityType::Table(TableType {
                element_type: RefType::FUNCREF,
                table64: false,
                minimum: 0,
                maximum: None,
                shared: false,
            }),
        );
    }
    for &h in &used {
        let ty = if h < 7 {
            1
        } else if h < BASE_HELPERS {
            2
        } else {
            3
        };
        imports.import("guest", MEM_HELPER_EXPORTS[h], EntityType::Function(ty));
    }
    module.section(&imports);

    // Functions: one local function, type index 0. Its function index comes
    // after the imported helpers.
    let block_func_idx = used.len() as u32;
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);

    // Exports: "block" -> the local function.
    let mut exports = ExportSection::new();
    exports.export("block", ExportKind::Func, block_func_idx);
    module.section(&exports);

    // Code. Locals: 1 (i64) = JALR new_pc stash (avoids rs1 == rd hazard),
    // 2 (i64) = mem-op effective address, 3 (i32) = TLB entry pointer,
    // 4-6 (i64) = MUL/DIV scratch (a, b, mulh sign-correction). 1 and 2
    // double as mulhu partial-product scratch — they're free between ops.
    // Chain mode appends 7 (i64) = next_pc, 8 (i32) = map-entry pointer.
    // Register lifting (Batch 6) appends one i64 local per lifted register
    // after all scratch/chain locals — base 7 (no chain) or 9 (chain).
    let lift_base: u32 = if chain.is_some() { 9 } else { 7 };
    let lift = if lift_flag {
        analyze_lift(&ops, lift_base)
    } else {
        Lift::empty()
    };
    let n_lift = lift.regs.len() as u32;

    let ctx = Ctx {
        start_pc,
        block_end_pc,
        helper_import_idx,
        tlb: tlb.copied(),
        lift,
    };

    let mut codes = CodeSection::new();
    let mut locals = vec![(2, ValType::I64), (1, ValType::I32), (3, ValType::I64)];
    if chain.is_some() {
        locals.push((1, ValType::I64));
        locals.push((1, ValType::I32));
    }
    if n_lift > 0 {
        locals.push((n_lift, ValType::I64));
    }
    let mut f = Function::new(locals);
    if let Some(c) = chain {
        // Self-charge at entry: s->n_cycles -= n_insns. Entry (not exit) so
        // MMU-fault bails stay charged too — same conservative overcharge
        // the C hook applied. The host's selector-20 ack told the hook to
        // stop charging on our behalf.
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I32Load(memarg32(c.n_cycles_off)));
        f.instruction(&Instruction::I32Const(n_insns as i32));
        f.instruction(&Instruction::I32Sub);
        f.instruction(&Instruction::I32Store(memarg32(c.n_cycles_off)));
    }
    // Lift prologue: prime locals from guest memory (invariant 3 in [Lift]).
    emit_entry_loads(&mut f, &ctx.lift);
    let ends_with_terminator = ops.last().map(Op::is_terminator).unwrap_or(false);
    emit_ops(&mut f, &ops, &ctx);
    // Spill lifted registers back to guest memory before any normal exit, so
    // the interpreter / chained next block / C dispatch loop sees fresh
    // values. Stack-neutral: a terminator's next_pc (already on the stack)
    // stays on top. The mid-block fault bail spills separately in fault_check.
    spill(&mut f, &ctx);
    // Non-terminator block: fall through to the static end-PC constant.
    // Terminator block: the last op already left next_pc on the stack.
    if !ends_with_terminator {
        f.instruction(&Instruction::I64Const(block_end_pc as i64));
    }
    // Chain mode: instead of returning next_pc to the C dispatch loop, try
    // to dispatch it ourselves. Fault bails (`Return` inside fault_check)
    // intentionally skip this — the interpreter must re-take the fault.
    if let Some(c) = chain {
        chain_epilogue(&mut f, c);
    }
    f.instruction(&Instruction::End);
    codes.function(&f);
    module.section(&codes);

    (module.finish(), used)
}

/// Translate one IR op to wasm instructions. All ops follow the pattern
/// `(load rs1?) (load rs2_or_imm) (compute) (store rd)`. Writes to x0
/// (reg_off == 0) are discarded by the C scanner before reaching here.
fn emit_ops(f: &mut Function, ops: &[Op], ctx: &Ctx) {
    for op in ops {
        emit_one(f, op, ctx);
    }
}

fn emit_one(f: &mut Function, op: &Op, ctx: &Ctx) {
    let block_end_pc = ctx.block_end_pc;
    use Op::*;
    match *op {
        // rd = imm (LUI, LI)
        Const { rd, imm } => {
            // store addr (lifted -> nothing); value; store.
            begin_store(f, rd, ctx);
            f.instruction(&Instruction::I64Const(imm));
            store_reg(f, rd, ctx);
        }

        // rd = rs1 + imm  (ADDI variants, also AUIPC where imm = pc + imm<<12)
        Addi { rd, rs1, imm } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            store_reg(f, rd, ctx);
        }
        // rd = sext32(rs1 + imm)
        Addiw { rd, rs1, imm } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        // rd = rs1 & imm
        Andi { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64And, ctx),
        Ori { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64Or, ctx),
        Xori { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64Xor, ctx),

        // Shifts (i-form, shift amount in imm)
        Slli { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64Shl);
            store_reg(f, rd, ctx);
        }
        Srli { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64ShrU);
            store_reg(f, rd, ctx);
        }
        Srai { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64ShrS);
            store_reg(f, rd, ctx);
        }
        Slliw { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32Shl);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        Srliw { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32ShrU);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        Sraiw { rd, rs1, shamt } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32ShrS);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        // rd = (rs1 < imm) signed/unsigned
        Slti { rd, rs1, imm } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64LtS);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd, ctx);
        }
        Sltiu { rd, rs1, imm } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64LtU);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd, ctx);
        }

        // R-form
        Add { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Add, ctx),
        Sub { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Sub, ctx),
        And { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64And, ctx),
        Or { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Or, ctx),
        Xor { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Xor, ctx),
        Sll { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64Shl);
            store_reg(f, rd, ctx);
        }
        Srl { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64ShrU);
            store_reg(f, rd, ctx);
        }
        Sra { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64ShrS);
            store_reg(f, rd, ctx);
        }
        Slt { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I64LtS);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd, ctx);
        }
        Sltu { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I64LtU);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd, ctx);
        }
        // 32-bit R-form: compute on low 32 bits, sign-extend to 64.
        Addw { rd, rs1, rs2 } => bin32(f, rd, rs1, rs2, &Instruction::I32Add, ctx),
        Subw { rd, rs1, rs2 } => bin32(f, rd, rs1, rs2, &Instruction::I32Sub, ctx),
        Sllw { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32Shl);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        Srlw { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32ShrU);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }
        Sraw { rd, rs1, rs2 } => {
            begin_store(f, rd, ctx);
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2, ctx);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32ShrS);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd, ctx);
        }

        // Terminators. Each leaves `next_pc: i64` on the stack as the
        // function return value. `block_end_pc` is the PC right after the
        // terminator insn — used as link_pc for JAL/JALR and as
        // fallthrough_pc for Bxx.
        Jal { rd, target_pc } => {
            if rd != 0 {
                begin_store(f, rd, ctx);
                f.instruction(&Instruction::I64Const(block_end_pc as i64));
                store_reg(f, rd, ctx);
            }
            f.instruction(&Instruction::I64Const(target_pc));
        }
        Jalr { rd, rs1, imm } => {
            // Compute new_pc = (reg[rs1] + imm) & ~1, stash in local 1 before
            // writing the link register, in case rd == rs1.
            load_reg(f, rs1, ctx);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            f.instruction(&Instruction::I64Const(-2)); // ~1 in i64
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::LocalSet(1));
            if rd != 0 {
                begin_store(f, rd, ctx);
                f.instruction(&Instruction::I64Const(block_end_pc as i64));
                store_reg(f, rd, ctx);
            }
            f.instruction(&Instruction::LocalGet(1));
        }
        Beq { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64Eq, ctx),
        Bne { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64Ne, ctx),
        Blt { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64LtS, ctx),
        Bge { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64GeS, ctx),
        Bltu { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64LtU, ctx),
        Bgeu { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64GeU, ctx),

        // Memory ops. With a TlbLayout (Batch 5): probe the guest's live TLB
        // inline — tag hit means a naturally aligned RAM access, performed
        // directly on the imported memory; miss falls back to the helper.
        // Without one: call the helper unconditionally (Batch 4 shape). The
        // helper performs the full insn semantics including rd writeback and
        // returns nonzero on MMU fault — the block then bails with the
        // faulting insn's pc, tagged with bit 0 so the interpreter
        // re-executes that insn and raises the exception. The inline fast
        // path cannot fault: a live TLB entry proves the page is mapped RAM
        // with this access type permitted. rd of a load is helper-written, so
        // it is never lifted (begin_store/store_reg here always hit memory).
        Load { w, rd, rs1, imm, pc_off } => {
            let fault_pc = ctx.start_pc + pc_off as u64;
            let helper = ctx.helper_import_idx[helper_id(op).unwrap()].unwrap();
            if let Some(t) = ctx.tlb {
                let bytes = match w {
                    LoadW::B | LoadW::Bu => 1,
                    LoadW::H | LoadW::Hu => 2,
                    LoadW::W | LoadW::Wu => 4,
                    LoadW::D => 8,
                };
                tlb_probe(f, t, rs1, imm, t.tlb_read_off, bytes, ctx);
                f.instruction(&Instruction::If(BlockType::Empty));
                // Fast: value = mem[addend + wrap32(addr)], width-extended.
                if rd != 0 {
                    begin_store(f, rd, ctx); // writeback base
                }
                tlb_host_addr(f, t, t.tlb_read_off);
                let arg = |align| MemArg { offset: 0, align, memory_index: 0 };
                f.instruction(&match w {
                    LoadW::B => Instruction::I64Load8S(arg(0)),
                    LoadW::Bu => Instruction::I64Load8U(arg(0)),
                    LoadW::H => Instruction::I64Load16S(arg(1)),
                    LoadW::Hu => Instruction::I64Load16U(arg(1)),
                    LoadW::W => Instruction::I64Load32S(arg(2)),
                    LoadW::Wu => Instruction::I64Load32U(arg(2)),
                    LoadW::D => Instruction::I64Load(arg(3)),
                });
                if rd != 0 {
                    store_reg(f, rd, ctx);
                } else {
                    // Load to x0: the access architecturally has no effect on
                    // RAM; we still perform it to stay shaped like the helper.
                    f.instruction(&Instruction::Drop);
                }
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::LocalGet(0)); // state
                f.instruction(&Instruction::LocalGet(2)); // addr
                f.instruction(&Instruction::I32Const(rd as i32)); // rd_off, 0 = x0
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc, ctx);
                f.instruction(&Instruction::End);
            } else {
                f.instruction(&Instruction::LocalGet(0)); // state
                load_reg(f, rs1, ctx);
                f.instruction(&Instruction::I64Const(imm));
                f.instruction(&Instruction::I64Add); // addr
                f.instruction(&Instruction::I32Const(rd as i32)); // rd_off, 0 = x0
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc, ctx);
            }
        }
        Store { w, rs1, rs2, imm, pc_off } => {
            let fault_pc = ctx.start_pc + pc_off as u64;
            let helper = ctx.helper_import_idx[helper_id(op).unwrap()].unwrap();
            if let Some(t) = ctx.tlb {
                let bytes = match w {
                    StoreW::B => 1,
                    StoreW::H => 2,
                    StoreW::W => 4,
                    StoreW::D => 8,
                };
                tlb_probe(f, t, rs1, imm, t.tlb_write_off, bytes, ctx);
                f.instruction(&Instruction::If(BlockType::Empty));
                tlb_host_addr(f, t, t.tlb_write_off);
                load_reg(f, rs2, ctx); // val
                let arg = |align| MemArg { offset: 0, align, memory_index: 0 };
                f.instruction(&match w {
                    StoreW::B => Instruction::I64Store8(arg(0)),
                    StoreW::H => Instruction::I64Store16(arg(1)),
                    StoreW::W => Instruction::I64Store32(arg(2)),
                    StoreW::D => Instruction::I64Store(arg(3)),
                });
                f.instruction(&Instruction::Else);
                f.instruction(&Instruction::LocalGet(0)); // state
                f.instruction(&Instruction::LocalGet(2)); // addr
                load_reg(f, rs2, ctx); // val
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc, ctx);
                f.instruction(&Instruction::End);
            } else {
                f.instruction(&Instruction::LocalGet(0)); // state
                load_reg(f, rs1, ctx);
                f.instruction(&Instruction::I64Const(imm));
                f.instruction(&Instruction::I64Add); // addr
                load_reg(f, rs2, ctx); // val
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc, ctx);
            }
        }

        // RV64M (Batch 4.5): inlined, see muldiv().
        MulDiv { op: md, rd, rs1, rs2 } => muldiv(f, md, rd, rs1, rs2, ctx),

        // RV64A (Batch 4.5): helper call, same shape as loads/stores. The
        // helper performs the full LR/SC/AMO semantics (load_res handling,
        // read-modify-write, rd writeback) and returns nonzero on MMU fault.
        // No displacement: the address is reg[rs1] directly. rd is
        // helper-written, so it is never lifted.
        Amo { d: _, rd, rs1, rs2, funct5, pc_off } => {
            let fault_pc = ctx.start_pc + pc_off as u64;
            let helper = ctx.helper_import_idx[helper_id(op).unwrap()].unwrap();
            f.instruction(&Instruction::LocalGet(0)); // state
            load_reg(f, rs1, ctx); // addr
            load_reg(f, rs2, ctx); // val2
            f.instruction(&Instruction::I32Const(funct5 as i32));
            f.instruction(&Instruction::I32Const(rd as i32)); // rd_off, 0 = x0
            f.instruction(&Instruction::Call(helper));
            fault_check(f, fault_pc, ctx);
        }
    }
}

/// RV64M codegen. Wasm's div/rem trap where RISC-V defines results (zero
/// divisor for all four; INT_MIN/-1 additionally for div_s), so those get
/// explicit guards reproducing the RISC-V values: div/0 = -1, rem/0 =
/// dividend, INT_MIN div -1 = INT_MIN (rem = 0 — which wasm's rem_s already
/// yields without trapping). mulh/mulhsu/mulhu synthesize the true 128-bit
/// high word — matching the interpreter, whose build defines HAVE_INT128 —
/// from mulhu partial products plus the sign-correction identities
/// mulh = mulhu(a,b) - ((a>>63)&b) - ((b>>63)&a), mulhsu = mulhu - ((a>>63)&b).
///
/// Locals 4/5 hold the operands (W-forms pre-narrowed so the 64-bit wasm op
/// gives the 32-bit result exactly); local 6 the mulh correction; mulhu_core
/// additionally clobbers 1/2 (free between ops).
fn muldiv(f: &mut Function, op: MdOp, rd: u32, rs1: u32, rs2: u32, ctx: &Ctx) {
    use Instruction::*;
    match op {
        MdOp::Mul => return bin_reg(f, rd, rs1, rs2, &I64Mul, ctx),
        MdOp::Mulw => return bin32(f, rd, rs1, rs2, &I32Mul, ctx),
        _ => {}
    }
    begin_store(f, rd, ctx); // store address for the result (lifted -> nothing)
    // Load operands into locals 4 (a) and 5 (b). W-forms narrow here: signed
    // ops sign-extend the low 32 bits, unsigned ops zero-extend, so the
    // 64-bit compare/divide below is exact 32-bit arithmetic.
    let narrow = match op {
        MdOp::Divw | MdOp::Remw => Some(true),
        MdOp::Divuw | MdOp::Remuw => Some(false),
        _ => None,
    };
    for (reg, local) in [(rs1, 4), (rs2, 5)] {
        load_reg(f, reg, ctx);
        if let Some(signed) = narrow {
            f.instruction(&I32WrapI64);
            f.instruction(if signed { &I64ExtendI32S } else { &I64ExtendI32U });
        }
        f.instruction(&LocalSet(local));
    }
    match op {
        MdOp::Mulhu => mulhu_core(f),
        MdOp::Mulh | MdOp::Mulhsu => {
            // l6 = ((a >> 63) & b) [+ ((b >> 63) & a) for mulh], computed
            // before mulhu_core clobbers the operands.
            f.instruction(&LocalGet(4));
            f.instruction(&I64Const(63));
            f.instruction(&I64ShrS);
            f.instruction(&LocalGet(5));
            f.instruction(&I64And);
            if op == MdOp::Mulh {
                f.instruction(&LocalGet(5));
                f.instruction(&I64Const(63));
                f.instruction(&I64ShrS);
                f.instruction(&LocalGet(4));
                f.instruction(&I64And);
                f.instruction(&I64Add);
            }
            f.instruction(&LocalSet(6));
            mulhu_core(f);
            f.instruction(&LocalGet(6));
            f.instruction(&I64Sub);
        }
        MdOp::Div | MdOp::Divw => {
            let min = if op == MdOp::Div { i64::MIN } else { i32::MIN as i64 };
            f.instruction(&LocalGet(5));
            f.instruction(&I64Eqz);
            f.instruction(&If(BlockType::Result(ValType::I64)));
            f.instruction(&I64Const(-1));
            f.instruction(&Else);
            f.instruction(&LocalGet(4));
            f.instruction(&I64Const(min));
            f.instruction(&I64Eq);
            f.instruction(&LocalGet(5));
            f.instruction(&I64Const(-1));
            f.instruction(&I64Eq);
            f.instruction(&I32And);
            f.instruction(&If(BlockType::Result(ValType::I64)));
            f.instruction(&LocalGet(4));
            f.instruction(&Else);
            // Guards exclude both wasm trap cases; Divw quotients always
            // fit int32 (the only overflow, MIN32/-1, took the branch above)
            // so the i64 value already equals its 32-bit sign-extension.
            f.instruction(&LocalGet(4));
            f.instruction(&LocalGet(5));
            f.instruction(&I64DivS);
            f.instruction(&End);
            f.instruction(&End);
        }
        MdOp::Divu | MdOp::Divuw => {
            f.instruction(&LocalGet(5));
            f.instruction(&I64Eqz);
            f.instruction(&If(BlockType::Result(ValType::I64)));
            f.instruction(&I64Const(-1)); // divu/0 = all ones; sext32 for W
            f.instruction(&Else);
            f.instruction(&LocalGet(4));
            f.instruction(&LocalGet(5));
            f.instruction(&I64DivU);
            if op == MdOp::Divuw {
                // Result is a uint32; RISC-V sign-extends it.
                f.instruction(&I32WrapI64);
                f.instruction(&I64ExtendI32S);
            }
            f.instruction(&End);
        }
        MdOp::Rem | MdOp::Remw => {
            // wasm rem_s only traps on zero divisor; MIN rem -1 is defined
            // as 0, which is exactly the RISC-V overflow value.
            f.instruction(&LocalGet(5));
            f.instruction(&I64Eqz);
            f.instruction(&If(BlockType::Result(ValType::I64)));
            f.instruction(&LocalGet(4)); // rem/0 = dividend
            f.instruction(&Else);
            f.instruction(&LocalGet(4));
            f.instruction(&LocalGet(5));
            f.instruction(&I64RemS);
            f.instruction(&End);
        }
        MdOp::Remu | MdOp::Remuw => {
            let w = op == MdOp::Remuw;
            f.instruction(&LocalGet(5));
            f.instruction(&I64Eqz);
            f.instruction(&If(BlockType::Result(ValType::I64)));
            f.instruction(&LocalGet(4)); // rem/0 = dividend (sext32 for W)
            if w {
                f.instruction(&I32WrapI64);
                f.instruction(&I64ExtendI32S);
            }
            f.instruction(&Else);
            f.instruction(&LocalGet(4));
            f.instruction(&LocalGet(5));
            f.instruction(&I64RemU);
            if w {
                f.instruction(&I32WrapI64);
                f.instruction(&I64ExtendI32S);
            }
            f.instruction(&End);
        }
        MdOp::Mul | MdOp::Mulw => unreachable!(),
    }
    store_reg(f, rd, ctx);
}

/// With the operands in locals 4 (a) and 5 (b), leave mulhu(a, b) — the high
/// 64 bits of the unsigned 128-bit product — on the stack. Four 32x32->64
/// partial products with carry propagation, the same algorithm as TinyEMU's
/// non-int128 mulhu fallback (whose carry handling is correct; only its mulh
/// sign-correction wrapper is buggy, and that path is dead under
/// HAVE_INT128 anyway). Clobbers locals 1, 2, 4, 5.
fn mulhu_core(f: &mut Function) {
    use Instruction::*;
    const M: i64 = 0xffff_ffff;
    // l1 = r01 = (a & M) * (b >> 32)
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&LocalGet(5));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&I64Mul);
    f.instruction(&LocalSet(1));
    // l2 = r10 = (a >> 32) * (b & M)
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&LocalGet(5));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Mul);
    f.instruction(&LocalSet(2));
    // stack: r00 >> 32 = ((a & M) * (b & M)) >> 32
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&LocalGet(5));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Mul);
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    // l4 = r11 = (a >> 32) * (b >> 32)   (a dead from here on)
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&LocalGet(5));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&I64Mul);
    f.instruction(&LocalSet(4));
    // c = (r00 >> 32) + (r01 & M) + (r10 & M)
    f.instruction(&LocalGet(1));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Add);
    f.instruction(&LocalGet(2));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Add);
    // l5 = c2 = (c >> 32) + (r01 >> 32) + (r10 >> 32) + (r11 & M)   (b dead)
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&LocalGet(1));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&I64Add);
    f.instruction(&LocalGet(2));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&I64Add);
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Add);
    f.instruction(&LocalTee(5));
    // result = ((c2 >> 32) + (r11 >> 32)) << 32 | (c2 & M)
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&LocalGet(4));
    f.instruction(&I64Const(32));
    f.instruction(&I64ShrU);
    f.instruction(&I64Add);
    f.instruction(&I64Const(32));
    f.instruction(&I64Shl);
    f.instruction(&LocalGet(5));
    f.instruction(&I64Const(M));
    f.instruction(&I64And);
    f.instruction(&I64Or);
}

/// Emit the shared front half of an inline TLB probe: compute the effective
/// address into local 2, the TLB entry pointer into local 3, and leave the
/// tag-compare result (i32 bool) on the stack. `tlb_off` selects tlb_read or
/// tlb_write. Mirrors the C fast path:
///   idx  = (addr >> PG_SHIFT) & (TLB_SIZE - 1)
///   hit  = tlb[idx].vaddr == (addr & ~(PG_MASK & ~(bytes - 1)))
fn tlb_probe(f: &mut Function, t: TlbLayout, rs1: u32, imm: i64, tlb_off: u32, bytes: u64, ctx: &Ctx) {
    // local 2 = addr = reg[rs1] + imm
    load_reg(f, rs1, ctx);
    f.instruction(&Instruction::I64Const(imm));
    f.instruction(&Instruction::I64Add);
    f.instruction(&Instruction::LocalTee(2));
    // local 3 = state + ((addr >> pg_shift) & idx_mask) << entry_shift
    f.instruction(&Instruction::I64Const(t.pg_shift as i64));
    f.instruction(&Instruction::I64ShrU);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Const(t.idx_mask as i32));
    f.instruction(&Instruction::I32And);
    f.instruction(&Instruction::I32Const(t.entry_shift as i32));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalTee(3));
    // tag compare
    f.instruction(&Instruction::I64Load(MemArg {
        offset: (tlb_off + t.vaddr_off) as u64,
        align: 3,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I64Const(tag_mask(t.pg_shift, bytes)));
    f.instruction(&Instruction::I64And);
    f.instruction(&Instruction::I64Eq);
}

/// Emit the fast-path host address: mem[entry.addend] + wrap32(addr), using
/// the locals tlb_probe established. Wrap-add reproduces the C macro's
/// `mem_addend + (uintptr_t)addr` on wasm32 exactly.
fn tlb_host_addr(f: &mut Function, t: TlbLayout, tlb_off: u32) {
    f.instruction(&Instruction::LocalGet(3));
    f.instruction(&Instruction::I32Load(MemArg {
        offset: (tlb_off + t.addend_off) as u64,
        align: 2,
        memory_index: 0,
    }));
    f.instruction(&Instruction::LocalGet(2));
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Add);
}

fn memarg32(offset: u32) -> MemArg {
    MemArg { offset: offset as u64, align: 2, memory_index: 0 }
}
fn memarg64(offset: u32) -> MemArg {
    MemArg { offset: offset as u64, align: 3, memory_index: 0 }
}

/// Chain epilogue (Batch 8). Stack on entry: [next_pc i64]. Replicates the
/// C-side jit_map_lookup checks (pc match + global/page/user generation
/// stamps) against the guest's live map in linear memory; on a validated hit
/// with cycle budget remaining, tail-calls the next compiled block through
/// the imported funcref table. Any check failing returns next_pc to the C
/// dispatch loop — exactly what the no-chain build does unconditionally.
///
/// Generation counters cannot change mid-chain (satp/CSR writes and fence.i
/// are scan terminators, executed only by the interpreter), so a stamp that
/// matches here is as fresh as one the C hook would have seen.
fn chain_epilogue(f: &mut Function, c: &ChainLayout) {
    let ret_next = |f: &mut Function| {
        f.instruction(&Instruction::If(BlockType::Empty));
        f.instruction(&Instruction::LocalGet(CHAIN_NEXT));
        f.instruction(&Instruction::Return);
        f.instruction(&Instruction::End);
    };
    f.instruction(&Instruction::LocalSet(CHAIN_NEXT));
    // Budget: if (s->n_cycles <= 0) return — lets the outer loop service
    // timer interrupts. The target block self-charges at its own entry.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I32Load(memarg32(c.n_cycles_off)));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32LeS);
    ret_next(f);
    // eptr = &map[((next_pc >> 1) * MULT) >> (64 - MAP_BITS)]
    f.instruction(&Instruction::LocalGet(CHAIN_NEXT));
    f.instruction(&Instruction::I64Const(1));
    f.instruction(&Instruction::I64ShrU);
    f.instruction(&Instruction::I64Const(HASH_MULT));
    f.instruction(&Instruction::I64Mul);
    f.instruction(&Instruction::I64Const(64 - c.map_bits as i64));
    f.instruction(&Instruction::I64ShrU);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Const(c.entry_size as i32));
    f.instruction(&Instruction::I32Mul);
    f.instruction(&Instruction::I32Const(c.map_base as i32));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::LocalTee(CHAIN_EPTR));
    // e->pc != next_pc -> miss
    f.instruction(&Instruction::I64Load(memarg64(0)));
    f.instruction(&Instruction::LocalGet(CHAIN_NEXT));
    f.instruction(&Instruction::I64Ne);
    ret_next(f);
    // e->global_gen != c2w_jit_global_gen -> stale
    f.instruction(&Instruction::LocalGet(CHAIN_EPTR));
    f.instruction(&Instruction::I32Load(memarg32(c.global_gen_off)));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Load(memarg32(c.global_gen_addr)));
    f.instruction(&Instruction::I32Ne);
    ret_next(f);
    // e->page_gen != c2w_jit_page_gen[((next_pc >> 12) * MULT) >> (64 - PG_BITS)]
    f.instruction(&Instruction::LocalGet(CHAIN_EPTR));
    f.instruction(&Instruction::I32Load(memarg32(c.page_gen_off)));
    f.instruction(&Instruction::LocalGet(CHAIN_NEXT));
    f.instruction(&Instruction::I64Const(PAGE_SHIFT));
    f.instruction(&Instruction::I64ShrU);
    f.instruction(&Instruction::I64Const(HASH_MULT));
    f.instruction(&Instruction::I64Mul);
    f.instruction(&Instruction::I64Const(64 - c.page_gen_bits as i64));
    f.instruction(&Instruction::I64ShrU);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(&Instruction::I32Const(2));
    f.instruction(&Instruction::I32Shl);
    f.instruction(&Instruction::I32Const(c.page_gen_base as i32));
    f.instruction(&Instruction::I32Add);
    f.instruction(&Instruction::I32Load(memarg32(0)));
    f.instruction(&Instruction::I32Ne);
    ret_next(f);
    // User-half pcs additionally check user_gen (kernel half is globally
    // mapped and survives satp rolls — same rule as jit_map_lookup).
    f.instruction(&Instruction::LocalGet(CHAIN_NEXT));
    f.instruction(&Instruction::I64Const(KERNEL_VA_BASE));
    f.instruction(&Instruction::I64LtU);
    f.instruction(&Instruction::If(BlockType::Empty));
    f.instruction(&Instruction::LocalGet(CHAIN_EPTR));
    f.instruction(&Instruction::I32Load(memarg32(c.user_gen_off)));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Load(memarg32(c.user_gen_addr)));
    f.instruction(&Instruction::I32Ne);
    ret_next(f);
    f.instruction(&Instruction::End);
    // c2w_jit_chain_hops++ (u64 stat the host reads at exit)
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I32Const(0));
    f.instruction(&Instruction::I64Load(memarg64(c.chain_hops_addr)));
    f.instruction(&Instruction::I64Const(1));
    f.instruction(&Instruction::I64Add);
    f.instruction(&Instruction::I64Store(memarg64(c.chain_hops_addr)));
    // Tail-call the next block: (state_ptr) through table[e->fn_idx]. Frame
    // is replaced, so arbitrarily long chains can't grow the wasm stack.
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::LocalGet(CHAIN_EPTR));
    f.instruction(&Instruction::I32Load(memarg32(c.fn_idx_off)));
    f.instruction(&Instruction::ReturnCallIndirect { type_index: 0, table_index: 0 });
}

/// Emit the post-helper fault check: nonzero return = MMU fault; bail out of
/// the block returning the faulting insn's pc with bit 0 set (real pcs are
/// even, so the tag is unambiguous to the dispatch hook).
fn fault_check(f: &mut Function, fault_pc: u64, ctx: &Ctx) {
    f.instruction(&Instruction::If(BlockType::Empty));
    // Commit lifted registers before bailing: the interpreter re-executes the
    // faulting insn and must see the values earlier ops in this block wrote.
    // Registers written only after the fault hold their entry-loaded memory
    // value (invariant 3 in [Lift]), so spilling them here is a no-op.
    spill(f, ctx);
    f.instruction(&Instruction::I64Const((fault_pc | 1) as i64));
    f.instruction(&Instruction::Return);
    f.instruction(&Instruction::End);
}

/// Conditional branch terminator. Emits explicit if/else producing target_pc
/// when the cmp holds, fallthrough_pc otherwise. (Earlier attempt used wasm
/// `select` over two i64 constants; that hung tight inner loops — kept the
/// if/else form to rule out any runtime-side polymorphic-select issue.)
fn branch(
    f: &mut Function,
    rs1: u32,
    rs2: u32,
    target_pc: i64,
    fallthrough_pc: u64,
    cmp: &Instruction,
    ctx: &Ctx,
) {
    load_reg(f, rs1, ctx);
    load_reg(f, rs2, ctx);
    f.instruction(cmp);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    f.instruction(&Instruction::I64Const(target_pc));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I64Const(fallthrough_pc as i64));
    f.instruction(&Instruction::End);
}

/// Push the store-address base for a subsequent `store_reg(rd)`. For a
/// memory-backed register that's the state pointer (so I64Store has its addr);
/// for a lifted register nothing (store_reg becomes a local.set). Must be
/// emitted in place of the leading `LocalGet(0)` that paired with store_reg.
fn begin_store(f: &mut Function, rd_off: u32, ctx: &Ctx) {
    if ctx.lift.local_of(rd_off).is_none() {
        f.instruction(&Instruction::LocalGet(0));
    }
}

fn load_reg(f: &mut Function, rs_off: u32, ctx: &Ctx) {
    if let Some(l) = ctx.lift.local_of(rs_off) {
        f.instruction(&Instruction::LocalGet(l));
    } else {
        f.instruction(&Instruction::LocalGet(0));
        f.instruction(&Instruction::I64Load(memarg64(rs_off)));
    }
}

fn store_reg(f: &mut Function, rd_off: u32, ctx: &Ctx) {
    if let Some(l) = ctx.lift.local_of(rd_off) {
        // Lifted: consume just the value. begin_store(rd) emitted nothing.
        f.instruction(&Instruction::LocalSet(l));
    } else {
        // Memory-backed: consume [addr i32, val i64] (addr from begin_store).
        f.instruction(&Instruction::I64Store(memarg64(rd_off)));
    }
}

fn bin_imm(f: &mut Function, rd: u32, rs1: u32, imm: i64, opcode: &Instruction, ctx: &Ctx) {
    begin_store(f, rd, ctx);
    load_reg(f, rs1, ctx);
    f.instruction(&Instruction::I64Const(imm));
    f.instruction(opcode);
    store_reg(f, rd, ctx);
}

fn bin_reg(f: &mut Function, rd: u32, rs1: u32, rs2: u32, opcode: &Instruction, ctx: &Ctx) {
    begin_store(f, rd, ctx);
    load_reg(f, rs1, ctx);
    load_reg(f, rs2, ctx);
    f.instruction(opcode);
    store_reg(f, rd, ctx);
}

fn bin32(f: &mut Function, rd: u32, rs1: u32, rs2: u32, opcode: &Instruction, ctx: &Ctx) {
    begin_store(f, rd, ctx);
    load_reg(f, rs1, ctx);
    f.instruction(&Instruction::I32WrapI64);
    load_reg(f, rs2, ctx);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(opcode);
    f.instruction(&Instruction::I64ExtendI32S);
    store_reg(f, rd, ctx);
}

/// Build a trivial wasm module used by the host pre-flight to validate the
/// wasm-encoder -> wasmtime compile -> instantiate -> call pipeline. Signature:
/// `(param i32) (result i64)`; body: `i64.const 42`. No imports.
pub fn build_preflight() -> Vec<u8> {
    let mut module = Module::new();
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I64]);
    module.section(&types);
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);
    let mut exports = ExportSection::new();
    exports.export("block", ExportKind::Func, 0);
    module.section(&exports);
    let mut codes = CodeSection::new();
    let mut f = Function::new(vec![]);
    f.instruction(&Instruction::I64Const(42));
    f.instruction(&Instruction::End);
    codes.function(&f);
    module.section(&codes);
    module.finish()
}
