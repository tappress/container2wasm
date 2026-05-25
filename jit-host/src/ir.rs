//! IR shared between the C scanner and the Rust codegen.
//!
//! Wire format is a flat byte stream of fixed-size tuples. The C side writes
//! it into a working-tree scratch buffer and passes (ptr, len) to
//! `jit.register_block`; the host reads `len` bytes and decodes here.
//!
//! Tuple = 16 bytes, little-endian, packed so both sides agree:
//!
//! ```text
//!   offset 0   u8     op_kind   (see OpKind below)
//!   offset 1   u8     _reserved
//!   offset 2   u16    rd_off    (byte offset of dest reg in RISCVCPUState)
//!   offset 4   u16    rs1_off   (or 0 if unused)
//!   offset 6   u16    rs2_off   (or 0 if unused)
//!   offset 8   i64    imm       (or shamt; sign-extended; bake-in-PC for AUIPC)
//! ```
//!
//! `rd_off == 0xFFFF` is the sentinel for "write to x0" — discarded by the C
//! emitter so we never see it here.

#[repr(u8)]
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub enum OpKind {
    Const = 1,  // rd = imm  (LUI / LI synthesizer)
    Addi = 2,
    Addiw = 3,
    Andi = 4,
    Ori = 5,
    Xori = 6,
    Slti = 7,
    Sltiu = 8,
    Slli = 9,
    Srli = 10,
    Srai = 11,
    Slliw = 12,
    Srliw = 13,
    Sraiw = 14,
    Add = 15,
    Sub = 16,
    And = 17,
    Or = 18,
    Xor = 19,
    Sll = 20,
    Srl = 21,
    Sra = 22,
    Slt = 23,
    Sltu = 24,
    Addw = 25,
    Subw = 26,
    Sllw = 27,
    Srlw = 28,
    Sraw = 29,
}

#[derive(Clone, Copy, Debug)]
pub enum Op {
    Const { rd: u32, imm: i64 },
    Addi { rd: u32, rs1: u32, imm: i64 },
    Addiw { rd: u32, rs1: u32, imm: i64 },
    Andi { rd: u32, rs1: u32, imm: i64 },
    Ori { rd: u32, rs1: u32, imm: i64 },
    Xori { rd: u32, rs1: u32, imm: i64 },
    Slti { rd: u32, rs1: u32, imm: i64 },
    Sltiu { rd: u32, rs1: u32, imm: i64 },
    Slli { rd: u32, rs1: u32, shamt: u32 },
    Srli { rd: u32, rs1: u32, shamt: u32 },
    Srai { rd: u32, rs1: u32, shamt: u32 },
    Slliw { rd: u32, rs1: u32, shamt: u32 },
    Srliw { rd: u32, rs1: u32, shamt: u32 },
    Sraiw { rd: u32, rs1: u32, shamt: u32 },
    Add { rd: u32, rs1: u32, rs2: u32 },
    Sub { rd: u32, rs1: u32, rs2: u32 },
    And { rd: u32, rs1: u32, rs2: u32 },
    Or { rd: u32, rs1: u32, rs2: u32 },
    Xor { rd: u32, rs1: u32, rs2: u32 },
    Sll { rd: u32, rs1: u32, rs2: u32 },
    Srl { rd: u32, rs1: u32, rs2: u32 },
    Sra { rd: u32, rs1: u32, rs2: u32 },
    Slt { rd: u32, rs1: u32, rs2: u32 },
    Sltu { rd: u32, rs1: u32, rs2: u32 },
    Addw { rd: u32, rs1: u32, rs2: u32 },
    Subw { rd: u32, rs1: u32, rs2: u32 },
    Sllw { rd: u32, rs1: u32, rs2: u32 },
    Srlw { rd: u32, rs1: u32, rs2: u32 },
    Sraw { rd: u32, rs1: u32, rs2: u32 },
}

pub const TUPLE_SIZE: usize = 16;

pub fn parse_ir(bytes: &[u8]) -> Vec<Op> {
    let mut ops = Vec::with_capacity(bytes.len() / TUPLE_SIZE);
    for chunk in bytes.chunks_exact(TUPLE_SIZE) {
        let op_kind = chunk[0];
        let rd = u16::from_le_bytes([chunk[2], chunk[3]]) as u32;
        let rs1 = u16::from_le_bytes([chunk[4], chunk[5]]) as u32;
        let rs2 = u16::from_le_bytes([chunk[6], chunk[7]]) as u32;
        let imm = i64::from_le_bytes([
            chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
        ]);
        let shamt = imm as u32;
        let op = match op_kind {
            x if x == OpKind::Const as u8 => Op::Const { rd, imm },
            x if x == OpKind::Addi as u8 => Op::Addi { rd, rs1, imm },
            x if x == OpKind::Addiw as u8 => Op::Addiw { rd, rs1, imm },
            x if x == OpKind::Andi as u8 => Op::Andi { rd, rs1, imm },
            x if x == OpKind::Ori as u8 => Op::Ori { rd, rs1, imm },
            x if x == OpKind::Xori as u8 => Op::Xori { rd, rs1, imm },
            x if x == OpKind::Slti as u8 => Op::Slti { rd, rs1, imm },
            x if x == OpKind::Sltiu as u8 => Op::Sltiu { rd, rs1, imm },
            x if x == OpKind::Slli as u8 => Op::Slli { rd, rs1, shamt },
            x if x == OpKind::Srli as u8 => Op::Srli { rd, rs1, shamt },
            x if x == OpKind::Srai as u8 => Op::Srai { rd, rs1, shamt },
            x if x == OpKind::Slliw as u8 => Op::Slliw { rd, rs1, shamt },
            x if x == OpKind::Srliw as u8 => Op::Srliw { rd, rs1, shamt },
            x if x == OpKind::Sraiw as u8 => Op::Sraiw { rd, rs1, shamt },
            x if x == OpKind::Add as u8 => Op::Add { rd, rs1, rs2 },
            x if x == OpKind::Sub as u8 => Op::Sub { rd, rs1, rs2 },
            x if x == OpKind::And as u8 => Op::And { rd, rs1, rs2 },
            x if x == OpKind::Or as u8 => Op::Or { rd, rs1, rs2 },
            x if x == OpKind::Xor as u8 => Op::Xor { rd, rs1, rs2 },
            x if x == OpKind::Sll as u8 => Op::Sll { rd, rs1, rs2 },
            x if x == OpKind::Srl as u8 => Op::Srl { rd, rs1, rs2 },
            x if x == OpKind::Sra as u8 => Op::Sra { rd, rs1, rs2 },
            x if x == OpKind::Slt as u8 => Op::Slt { rd, rs1, rs2 },
            x if x == OpKind::Sltu as u8 => Op::Sltu { rd, rs1, rs2 },
            x if x == OpKind::Addw as u8 => Op::Addw { rd, rs1, rs2 },
            x if x == OpKind::Subw as u8 => Op::Subw { rd, rs1, rs2 },
            x if x == OpKind::Sllw as u8 => Op::Sllw { rd, rs1, rs2 },
            x if x == OpKind::Srlw as u8 => Op::Srlw { rd, rs1, rs2 },
            x if x == OpKind::Sraw as u8 => Op::Sraw { rd, rs1, rs2 },
            other => panic!("unknown IR op_kind {}", other),
        };
        ops.push(op);
    }
    ops
}
