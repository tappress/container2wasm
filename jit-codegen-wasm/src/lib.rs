//! Browser build of the jit-host block codegen.
//!
//! Compiles the exact same codegen.rs/ir.rs the wasmtime embedder uses
//! (#[path] includes, so output is byte-identical) to wasm32-unknown-unknown
//! with a C-ABI surface the JS coordinator drives:
//!
//!   jcg_in_ptr(len)   -> ptr   reserve + expose the IR input buffer
//!   jcg_build(ir_len, start_pc, end_pc, n_insns) -> module byte length
//!   jcg_out_ptr()     -> ptr   the compiled module bytes
//!   jcg_helpers_ptr() -> ptr   used helper ids (u8 each), jcg_helpers_len()
//!   jcg_set_tlb(...)  / jcg_set_chain(...)   opt-in layouts, sticky
//!
//! Single-threaded by construction (one worker), so plain statics are fine.

#[path = "../../jit-host/src/ir.rs"]
pub mod ir;
#[path = "../../jit-host/src/codegen.rs"]
pub mod codegen;

use codegen::{build_block, ChainLayout, TlbLayout};

static mut IN_BUF: Vec<u8> = Vec::new();
static mut OUT_MODULE: Vec<u8> = Vec::new();
static mut OUT_HELPERS: Vec<u8> = Vec::new();
static mut TLB: Option<TlbLayout> = None;
static mut CHAIN: Option<ChainLayout> = None;

#[allow(static_mut_refs)]
#[no_mangle]
pub extern "C" fn jcg_in_ptr(len: u32) -> *mut u8 {
    unsafe {
        IN_BUF.clear();
        IN_BUF.resize(len as usize, 0);
        IN_BUF.as_mut_ptr()
    }
}

#[allow(static_mut_refs)]
#[no_mangle]
pub extern "C" fn jcg_build(ir_len: u32, start_pc: u64, end_pc: u64, n_insns: u32) -> u32 {
    unsafe {
        let ir = &IN_BUF[..ir_len as usize];
        let (bytes, helpers) =
            build_block(ir, start_pc, end_pc, n_insns, TLB.as_ref(), CHAIN.as_ref());
        OUT_MODULE = bytes;
        OUT_HELPERS = helpers.iter().map(|&h| h as u8).collect();
        OUT_MODULE.len() as u32
    }
}

#[allow(static_mut_refs)]
#[no_mangle]
pub extern "C" fn jcg_out_ptr() -> *const u8 {
    unsafe { OUT_MODULE.as_ptr() }
}

#[allow(static_mut_refs)]
#[no_mangle]
pub extern "C" fn jcg_helpers_ptr() -> *const u8 {
    unsafe { OUT_HELPERS.as_ptr() }
}

#[allow(static_mut_refs)]
#[no_mangle]
pub extern "C" fn jcg_helpers_len() -> u32 {
    unsafe { OUT_HELPERS.len() as u32 }
}

#[no_mangle]
pub extern "C" fn jcg_set_tlb(
    tlb_read_off: u32,
    tlb_write_off: u32,
    idx_mask: u32,
    entry_shift: u32,
    vaddr_off: u32,
    addend_off: u32,
    pg_shift: u32,
) {
    unsafe {
        TLB = Some(TlbLayout {
            tlb_read_off,
            tlb_write_off,
            idx_mask,
            entry_shift,
            vaddr_off,
            addend_off,
            pg_shift,
        });
    }
}

#[no_mangle]
pub extern "C" fn jcg_set_chain(
    n_cycles_off: u32,
    map_base: u32,
    entry_size: u32,
    fn_idx_off: u32,
    user_gen_off: u32,
    global_gen_off: u32,
    page_gen_off: u32,
    map_bits: u32,
    page_gen_bits: u32,
    user_gen_addr: u32,
    global_gen_addr: u32,
    page_gen_base: u32,
    chain_hops_addr: u32,
) {
    unsafe {
        CHAIN = Some(ChainLayout {
            n_cycles_off,
            map_base,
            entry_size,
            fn_idx_off,
            user_gen_off,
            global_gen_off,
            page_gen_off,
            map_bits,
            page_gen_bits,
            user_gen_addr,
            global_gen_addr,
            page_gen_base,
            chain_hops_addr,
        });
    }
}
