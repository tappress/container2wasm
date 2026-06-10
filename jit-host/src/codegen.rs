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
pub fn build_block(ir: &[u8], start_pc: u64, block_end_pc: u64) -> (Vec<u8>, Vec<usize>) {
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
    };

    // Code. One i64 local (idx 1) used by JALR codegen to stash new_pc before
    // performing the link-register write (avoids rs1 == rd hazard).
    let mut codes = CodeSection::new();
    let mut f = Function::new(vec![(1, ValType::I64)]);
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

        // Memory ops: call the imported guest helper, which performs the
        // access (TLB fast path inline in the guest module) including rd
        // writeback for loads. Helper returns nonzero on MMU fault — the
        // block then bails with the faulting insn's pc, tagged with bit 0 so
        // the interpreter re-executes that insn and raises the exception.
        Load { w: _, rd, rs1, imm, pc_off } => {
            f.instruction(&Instruction::LocalGet(0)); // state
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add); // addr
            f.instruction(&Instruction::I32Const(rd as i32)); // rd_off, 0 = x0
            f.instruction(&Instruction::Call(
                ctx.helper_import_idx[helper_id(op).unwrap()].unwrap(),
            ));
            fault_check(f, ctx.start_pc + pc_off as u64);
        }
        Store { w: _, rs1, rs2, imm, pc_off } => {
            f.instruction(&Instruction::LocalGet(0)); // state
            load_reg(f, rs1);
            f.instruction(&Instruction::I64Const(imm));
            f.instruction(&Instruction::I64Add); // addr
            load_reg(f, rs2); // val
            f.instruction(&Instruction::Call(
                ctx.helper_import_idx[helper_id(op).unwrap()].unwrap(),
            ));
            fault_check(f, ctx.start_pc + pc_off as u64);
        }
    }
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
