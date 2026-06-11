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

use crate::ir::{LoadW, Op, StoreW, parse_ir};
use wasm_encoder::*;

/// Guest export names of the memory helpers, indexed by helper id (loads
/// 0-6 = lb/lh/lw/ld/lbu/lhu/lwu, stores 7-10 = sb/sh/sw/sd). main.rs
/// resolves these from the c2w instance and passes them as block imports
/// positionally, so the order here is load-bearing.
pub const MEM_HELPER_EXPORTS: [&str; 11] = [
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
];

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
        _ => None,
    }
}

/// Per-block codegen context: constants baked into emitted code plus the
/// mapping from helper id to this module's function-import index.
struct Ctx {
    start_pc: u64,
    block_end_pc: u64,
    helper_import_idx: [Option<u32>; 11],
    tlb: Option<TlbLayout>,
}

/// Build a wasm module containing one exported function "block" with signature
/// `(state_ptr: i32) -> (next_pc: i64)` implementing the given IR sequence.
///
/// `start_pc` is the block's first guest PC — memory ops bake
/// `start_pc + pc_off` as their tagged fault-return value, so the content
/// cache must key on it. `block_end_pc` is the absolute guest PC at which the
/// compiled run ends.
///
/// Returns the module bytes plus the helper ids the block imports, in import
/// order — the caller must pass exactly those guest funcs (after the memory)
/// to instantiation.
pub fn build_block(
    ir: &[u8],
    start_pc: u64,
    block_end_pc: u64,
    tlb: Option<&TlbLayout>,
) -> (Vec<u8>, Vec<usize>) {
    let ops = parse_ir(ir);

    // Which memory helpers does this block use? Imports are declared only
    // for those (ascending helper id), so pure ALU/branch blocks keep the
    // old single-memory-import shape and work against guests that don't
    // export the helpers.
    let mut used: Vec<usize> = ops.iter().filter_map(helper_id).collect();
    used.sort_unstable();
    used.dedup();
    let mut helper_import_idx = [None; 11];
    for (i, &h) in used.iter().enumerate() {
        helper_import_idx[h] = Some(i as u32);
    }

    let mut module = Module::new();

    // Types: 0 = block (i32)->(i64), 1 = load helper (state, addr, rd_off)
    // -> fault, 2 = store helper (state, addr, val) -> fault. 1 and 2 are
    // emitted unconditionally so indices stay fixed.
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I64]);
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64, ValType::I32], vec![ValType::I32]);
    types
        .ty()
        .function(vec![ValType::I32, ValType::I64, ValType::I64], vec![ValType::I32]);
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
    for &h in &used {
        let ty = if h < 7 { 1 } else { 2 };
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

    let ctx = Ctx {
        start_pc,
        block_end_pc,
        helper_import_idx,
        tlb: tlb.copied(),
    };

    // Code. Locals: 1 (i64) = JALR new_pc stash (avoids rs1 == rd hazard),
    // 2 (i64) = mem-op effective address, 3 (i32) = TLB entry pointer.
    let mut codes = CodeSection::new();
    let mut f = Function::new(vec![(2, ValType::I64), (1, ValType::I32)]);
    let ends_with_terminator = ops.last().map(Op::is_terminator).unwrap_or(false);
    emit_ops(&mut f, &ops, &ctx);
    // Non-terminator block: fall through to the static end-PC constant.
    // Terminator block: the last op already left next_pc on the stack.
    if !ends_with_terminator {
        f.instruction(&Instruction::I64Const(block_end_pc as i64));
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
            // state_ptr on stack as store addr; value; store.
            f.instruction(&Instruction::LocalGet(0));
            f.instruction(&Instruction::I64Const(imm));
            store_reg(f, rd);
        }

        // rd = rs1 + imm  (ADDI variants, also AUIPC where imm = pc + imm<<12)
        Addi { rd, rs1, imm } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            store_reg(f, rd);
        }
        // rd = sext32(rs1 + imm)
        Addiw { rd, rs1, imm } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        // rd = rs1 & imm
        Andi { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64And),
        Ori { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64Or),
        Xori { rd, rs1, imm } => bin_imm(f, rd, rs1, imm, &Instruction::I64Xor),

        // Shifts (i-form, shift amount in imm)
        Slli { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64Shl);
            store_reg(f, rd);
        }
        Srli { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64ShrU);
            store_reg(f, rd);
        }
        Srai { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(shamt as i64));
            f.instruction(&Instruction::I64ShrS);
            store_reg(f, rd);
        }
        Slliw { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32Shl);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        Srliw { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32ShrU);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        Sraiw { rd, rs1, shamt } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(shamt as i32));
            f.instruction(&Instruction::I32ShrS);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        // rd = (rs1 < imm) signed/unsigned
        Slti { rd, rs1, imm } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64LtS);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd);
        }
        Sltiu { rd, rs1, imm } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64LtU);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd);
        }

        // R-form
        Add { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Add),
        Sub { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Sub),
        And { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64And),
        Or { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Or),
        Xor { rd, rs1, rs2 } => bin_reg(f, rd, rs1, rs2, &Instruction::I64Xor),
        Sll { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            load_reg(f, rs2);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64Shl);
            store_reg(f, rd);
        }
        Srl { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            load_reg(f, rs2);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64ShrU);
            store_reg(f, rd);
        }
        Sra { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            load_reg(f, rs2);
            f.instruction(&Instruction::I64Const(0x3f));
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::I64ShrS);
            store_reg(f, rd);
        }
        Slt { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            load_reg(f, rs2);
            f.instruction(&Instruction::I64LtS);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd);
        }
        Sltu { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            load_reg(f, rs2);
            f.instruction(&Instruction::I64LtU);
            f.instruction(&Instruction::I64ExtendI32U);
            store_reg(f, rd);
        }
        // 32-bit R-form: compute on low 32 bits, sign-extend to 64.
        Addw { rd, rs1, rs2 } => bin32(f, rd, rs1, rs2, &Instruction::I32Add),
        Subw { rd, rs1, rs2 } => bin32(f, rd, rs1, rs2, &Instruction::I32Sub),
        Sllw { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32Shl);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        Srlw { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32ShrU);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }
        Sraw { rd, rs1, rs2 } => {
            f.instruction(&Instruction::LocalGet(0));
            load_reg(f, rs1);
            f.instruction(&Instruction::I32WrapI64);
            load_reg(f, rs2);
            f.instruction(&Instruction::I32WrapI64);
            f.instruction(&Instruction::I32Const(0x1f));
            f.instruction(&Instruction::I32And);
            f.instruction(&Instruction::I32ShrS);
            f.instruction(&Instruction::I64ExtendI32S);
            store_reg(f, rd);
        }

        // Terminators. Each leaves `next_pc: i64` on the stack as the
        // function return value. `block_end_pc` is the PC right after the
        // terminator insn — used as link_pc for JAL/JALR and as
        // fallthrough_pc for Bxx.
        Jal { rd, target_pc } => {
            if rd != 0 {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64Const(block_end_pc as i64));
                store_reg(f, rd);
            }
            f.instruction(&Instruction::I64Const(target_pc));
        }
        Jalr { rd, rs1, imm } => {
            // Compute new_pc = (reg[rs1] + imm) & ~1, stash in local 1 before
            // writing the link register, in case rd == rs1.
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add);
            f.instruction(&Instruction::I64Const(-2)); // ~1 in i64
            f.instruction(&Instruction::I64And);
            f.instruction(&Instruction::LocalSet(1));
            if rd != 0 {
                f.instruction(&Instruction::LocalGet(0));
                f.instruction(&Instruction::I64Const(block_end_pc as i64));
                store_reg(f, rd);
            }
            f.instruction(&Instruction::LocalGet(1));
        }
        Beq { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64Eq),
        Bne { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64Ne),
        Blt { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64LtS),
        Bge { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64GeS),
        Bltu { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64LtU),
        Bgeu { rs1, rs2, target_pc } => branch(f, rs1, rs2, target_pc, block_end_pc, &Instruction::I64GeU),

        // Memory ops. With a TlbLayout (Batch 5): probe the guest's live TLB
        // inline — tag hit means a naturally aligned RAM access, performed
        // directly on the imported memory; miss falls back to the helper.
        // Without one: call the helper unconditionally (Batch 4 shape). The
        // helper performs the full insn semantics including rd writeback and
        // returns nonzero on MMU fault — the block then bails with the
        // faulting insn's pc, tagged with bit 0 so the interpreter
        // re-executes that insn and raises the exception. The inline fast
        // path cannot fault: a live TLB entry proves the page is mapped RAM
        // with this access type permitted.
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
                tlb_probe(f, t, rs1, imm, t.tlb_read_off, bytes);
                f.instruction(&Instruction::If(BlockType::Empty));
                // Fast: value = mem[addend + wrap32(addr)], width-extended.
                if rd != 0 {
                    f.instruction(&Instruction::LocalGet(0)); // writeback base
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
                    store_reg(f, rd);
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
                fault_check(f, fault_pc);
                f.instruction(&Instruction::End);
            } else {
                f.instruction(&Instruction::LocalGet(0)); // state
                load_reg(f, rs1);
                f.instruction(&Instruction::I64Const(imm));
                f.instruction(&Instruction::I64Add); // addr
                f.instruction(&Instruction::I32Const(rd as i32)); // rd_off, 0 = x0
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc);
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
                tlb_probe(f, t, rs1, imm, t.tlb_write_off, bytes);
                f.instruction(&Instruction::If(BlockType::Empty));
                tlb_host_addr(f, t, t.tlb_write_off);
                load_reg(f, rs2); // val
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
                load_reg(f, rs2); // val
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc);
                f.instruction(&Instruction::End);
            } else {
                f.instruction(&Instruction::LocalGet(0)); // state
                load_reg(f, rs1);
                f.instruction(&Instruction::I64Const(imm));
                f.instruction(&Instruction::I64Add); // addr
                load_reg(f, rs2); // val
                f.instruction(&Instruction::Call(helper));
                fault_check(f, fault_pc);
            }
        }
    }
}

/// Emit the shared front half of an inline TLB probe: compute the effective
/// address into local 2, the TLB entry pointer into local 3, and leave the
/// tag-compare result (i32 bool) on the stack. `tlb_off` selects tlb_read or
/// tlb_write. Mirrors the C fast path:
///   idx  = (addr >> PG_SHIFT) & (TLB_SIZE - 1)
///   hit  = tlb[idx].vaddr == (addr & ~(PG_MASK & ~(bytes - 1)))
fn tlb_probe(f: &mut Function, t: TlbLayout, rs1: u32, imm: i64, tlb_off: u32, bytes: u64) {
    // local 2 = addr = reg[rs1] + imm
    load_reg(f, rs1);
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

/// Emit the post-helper fault check: nonzero return = MMU fault; bail out of
/// the block returning the faulting insn's pc with bit 0 set (real pcs are
/// even, so the tag is unambiguous to the dispatch hook).
fn fault_check(f: &mut Function, fault_pc: u64) {
    f.instruction(&Instruction::If(BlockType::Empty));
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
) {
    load_reg(f, rs1);
    load_reg(f, rs2);
    f.instruction(cmp);
    f.instruction(&Instruction::If(BlockType::Result(ValType::I64)));
    f.instruction(&Instruction::I64Const(target_pc));
    f.instruction(&Instruction::Else);
    f.instruction(&Instruction::I64Const(fallthrough_pc as i64));
    f.instruction(&Instruction::End);
}

fn load_reg(f: &mut Function, rs_off: u32) {
    f.instruction(&Instruction::LocalGet(0));
    f.instruction(&Instruction::I64Load(MemArg {
        offset: rs_off as u64,
        align: 3,
        memory_index: 0,
    }));
}

fn store_reg(f: &mut Function, rd_off: u32) {
    // expects [addr i32, val i64] on stack from caller.
    f.instruction(&Instruction::I64Store(MemArg {
        offset: rd_off as u64,
        align: 3,
        memory_index: 0,
    }));
}

fn bin_imm(f: &mut Function, rd: u32, rs1: u32, imm: i64, opcode: &Instruction) {
    f.instruction(&Instruction::LocalGet(0));
    load_reg(f, rs1);
    f.instruction(&Instruction::I64Const(imm));
    f.instruction(opcode);
    store_reg(f, rd);
}

fn bin_reg(f: &mut Function, rd: u32, rs1: u32, rs2: u32, opcode: &Instruction) {
    f.instruction(&Instruction::LocalGet(0));
    load_reg(f, rs1);
    load_reg(f, rs2);
    f.instruction(opcode);
    store_reg(f, rd);
}

fn bin32(f: &mut Function, rd: u32, rs1: u32, rs2: u32, opcode: &Instruction) {
    f.instruction(&Instruction::LocalGet(0));
    load_reg(f, rs1);
    f.instruction(&Instruction::I32WrapI64);
    load_reg(f, rs2);
    f.instruction(&Instruction::I32WrapI64);
    f.instruction(opcode);
    f.instruction(&Instruction::I64ExtendI32S);
    store_reg(f, rd);
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
