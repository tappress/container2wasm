//! Host embedder for the c2w JIT.
//!
//! Replaces `wasmtime run --preload jit=... --preload c2w_blk=...` so we can
//! mutate the block table at runtime (which static --preload modules can't).
//!
//! Current state: minimum-viable embedder. Provides:
//!   - WASI preview1
//!   - c2w_blk.read/write stubs (return -1)
//!   - jit.dispatch_noop / jit.dispatch_indirect — same semantics as the
//!     hand-written helper wasm used during Batch 1
//!   - jit.register_block stub (Batch 2 codegen will fill it in)
//!
//! Next step (Batch 2): on register_block, read bytes from guest memory,
//! compile the wasm module, store the resulting Func in state.blocks keyed
//! by guest PC. dispatch_indirect looks up and calls.

use std::collections::HashMap;
use wasmtime::*;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

struct HostState {
    wasi: WasiP1Ctx,
    blocks: HashMap<u64, Func>,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <c2w.wasm> [guest args...]", args[0]);
        std::process::exit(2);
    }
    let wasm_path = &args[1];
    let guest_args = &args[2..];

    let engine = Engine::default();
    let module = Module::from_file(&engine, wasm_path)?;

    let mut linker: Linker<HostState> = Linker::new(&engine);
    wasmtime_wasi::p1::add_to_linker_sync(&mut linker, |s| &mut s.wasi)?;

    linker.func_wrap(
        "c2w_blk",
        "read",
        |_drv: i32, _off: i64, _ptr: i32, _len: i32| -> i32 { -1 },
    )?;
    linker.func_wrap(
        "c2w_blk",
        "write",
        |_drv: i32, _off: i64, _ptr: i32, _len: i32| -> i32 { -1 },
    )?;

    linker.func_wrap(
        "jit",
        "dispatch_noop",
        |_pc: i64, _state_ptr: i32| -> i64 { 0 },
    )?;
    linker.func_wrap(
        "jit",
        "dispatch_indirect",
        |mut caller: Caller<'_, HostState>, pc: i64, state_ptr: i32| -> i64 {
            let func = caller.data().blocks.get(&(pc as u64)).copied();
            match func {
                Some(f) => {
                    let typed = f
                        .typed::<i32, i64>(&caller)
                        .expect("compiled block has wrong signature");
                    typed.call(&mut caller, state_ptr).unwrap_or(0)
                }
                None => 0,
            }
        },
    )?;
    linker.func_wrap(
        "jit",
        "register_block",
        |_pc: i64, _bytes_ptr: i32, _bytes_len: i32| {
            /* Batch 2 will fill this in. */
        },
    )?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdio();
    wasi_builder.arg("c2w");
    for a in guest_args {
        wasi_builder.arg(a);
    }
    let wasi = wasi_builder.build_p1();

    let state = HostState {
        wasi,
        blocks: HashMap::new(),
    };
    let mut store = Store::new(&engine, state);

    let instance = linker.instantiate(&mut store, &module)?;
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    match start.call(&mut store, ()) {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                std::process::exit(exit.0);
            }
            Err(e)
        }
    }
}
