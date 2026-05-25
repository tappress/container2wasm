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

use crate::ir::{Op, parse_ir};
use wasm_encoder::*;

/// Build a wasm module containing one exported function "block" with signature
/// `(state_ptr: i32) -> (next_pc: i64)` implementing the given IR sequence.
///
/// `block_end_pc` is the absolute guest PC at which the compiled run ends
/// (i.e. PC of the first non-ALU insn). The compiled block stores this into
/// `s->pc` via the host-imported state pointer and returns it.
pub fn build_block(ir: &[u8], block_end_pc: u64) -> Vec<u8> {
    let ops = parse_ir(ir);

    let mut module = Module::new();

    // Types: one function type (i32) -> (i64).
    let mut types = TypeSection::new();
    types.ty().function(vec![ValType::I32], vec![ValType::I64]);
    module.section(&types);

    // Imports: guest.mem (we don't actually need any min pages; the host
    // overrides with the real guest memory at instantiate time).
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
    module.section(&imports);

    // Functions: one local function, type index 0.
    let mut funcs = FunctionSection::new();
    funcs.function(0);
    module.section(&funcs);

    // Exports: "block" -> function 0.
    let mut exports = ExportSection::new();
    exports.export("block", ExportKind::Func, 0);
    module.section(&exports);

    // Code.
    let mut codes = CodeSection::new();
    let mut f = Function::new(vec![]);
    emit_ops(&mut f, &ops);
    // Emit the block-exit PC and return.
    f.instruction(&Instruction::I64Const(block_end_pc as i64));
    f.instruction(&Instruction::End);
    codes.function(&f);
    module.section(&codes);

    module.finish()
}

/// Translate one IR op to wasm instructions. All ops follow the pattern
/// `(load rs1?) (load rs2_or_imm) (compute) (store rd)`. Writes to x0
/// (reg_off == 0) are discarded by the C scanner before reaching here.
fn emit_ops(f: &mut Function, ops: &[Op]) {
    for op in ops {
        emit_one(f, op);
    }
}

fn emit_one(f: &mut Function, op: &Op) {
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
    }
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
