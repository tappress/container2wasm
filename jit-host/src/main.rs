//! Host embedder for the c2w JIT.
//!
//! Replaces `wasmtime run --preload jit=... --preload c2w_blk=...` so we can
//! mutate the block table at runtime — which static --preload modules can't.
//!
//! Provides:
//!   - WASI preview1
//!   - c2w_blk.read/write stubs (return -1)
//!   - jit.dispatch_noop   — measurement-mode no-op
//!   - jit.dispatch_indirect — looks up Func in HostState.blocks, calls it
//!   - jit.register_block  — reads IR bytes from guest memory, codegens a
//!                            wasm module via wasm-encoder, compiles it, stores
//!                            the resulting Func keyed by guest PC
//!   - jit.mark_uncompilable — sentinel for "this PC has no eligible block"
//!                              so the C scanner doesn't keep re-trying

mod codegen;
mod ir;

use std::collections::HashMap;
use wasmtime::*;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

struct HostState {
    wasi: WasiP1Ctx,
    /// Compiled-block table keyed by guest PC. `Some(Some(_))` = compiled,
    /// `Some(None)` = "uncompilable" sentinel (don't retry).
    blocks: HashMap<u64, Option<TypedFunc<i32, i64>>>,
    /// Cached handle to the guest module's exported linear memory.
    guest_memory: Option<Memory>,
    n_register_ok: u64,
    n_register_fail: u64,
    n_dispatch_hit: u64,
    n_dispatch_miss: u64,
    compile_nanos: u64,
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

    // Pre-flight: build, compile, instantiate, and invoke a trivial dynamic
    // wasm module to verify the wasm-encoder -> wasmtime pipeline before we
    // depend on it during guest execution.
    {
        let bytes = codegen::build_preflight();
        let m = Module::from_binary(&engine, &bytes)?;
        let mut probe_store: Store<()> = Store::new(&engine, ());
        let inst = Instance::new(&mut probe_store, &m, &[])?;
        let f = inst.get_typed_func::<i32, i64>(&mut probe_store, "block")?;
        let r = f.call(&mut probe_store, 0)?;
        assert_eq!(r, 42, "preflight block returned {r}, expected 42");
        eprintln!("[jit-host] preflight OK: dynamic module compile+call works");
    }

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
            let typed = match caller.data().blocks.get(&(pc as u64)) {
                Some(Some(f)) => f.clone(),
                _ => {
                    caller.data_mut().n_dispatch_miss += 1;
                    return 0;
                }
            };
            let r = typed.call(&mut caller, state_ptr).unwrap_or(0);
            caller.data_mut().n_dispatch_hit += 1;
            r
        },
    )?;
    let disable_compile = std::env::var_os("JIT_DISABLE_COMPILE").is_some();
    let max_blocks: u64 = std::env::var("JIT_MAX_BLOCKS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u64::MAX);
    let skip_pcs: std::collections::HashSet<u64> = std::env::var("JIT_SKIP_PCS")
        .ok()
        .map(|s| {
            s.split(',')
                .filter_map(|t| u64::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
                .collect()
        })
        .unwrap_or_default();
    linker.func_wrap(
        "jit",
        "register_block",
        move |mut caller: Caller<'_, HostState>,
              pc: i64,
              end_pc: i64,
              ir_ptr: i32,
              ir_len: i32|
              -> i32 {
            if disable_compile
                || caller.data().n_register_ok >= max_blocks
                || skip_pcs.contains(&(pc as u64))
            {
                caller.data_mut().blocks.insert(pc as u64, None);
                return 0;
            }
            let t0 = std::time::Instant::now();
            let r = match register_block_inner(&mut caller, pc as u64, end_pc as u64, ir_ptr, ir_len) {
                Ok(()) => {
                    caller.data_mut().n_register_ok += 1;
                    1
                }
                Err(e) => {
                    if std::env::var_os("JIT_LOG_FAIL").is_some() {
                        eprintln!("[jit-host] register_block failed @ pc=0x{:x}: {e}", pc);
                    }
                    let st = caller.data_mut();
                    st.blocks.insert(pc as u64, None);
                    st.n_register_fail += 1;
                    0
                }
            };
            caller.data_mut().compile_nanos += t0.elapsed().as_nanos() as u64;
            r
        },
    )?;
    linker.func_wrap(
        "jit",
        "mark_uncompilable",
        |mut caller: Caller<'_, HostState>, pc: i64| {
            caller.data_mut().blocks.insert(pc as u64, None);
        },
    )?;

    let mut wasi_builder = WasiCtxBuilder::new();
    wasi_builder.inherit_stdout().inherit_stderr();
    // c2w's main parses argv: [program, c2w-flags..., guest-command...].
    // `--no-stdin` tells temu.c to skip its stdin poll loop during boot —
    // without it, poll_oneoff on a non-TTY host stdin returns EINVAL and the
    // kernel exits before runc launches the container command. The wasmtime
    // CLI repro passes the same flag explicitly. We inject it unconditionally;
    // callers don't have a stdin-to-guest path yet.
    wasi_builder.arg("c2w");
    wasi_builder.arg("--no-stdin");
    for a in guest_args {
        wasi_builder.arg(a);
    }
    let wasi = wasi_builder.build_p1();

    let state = HostState {
        wasi,
        blocks: HashMap::new(),
        guest_memory: None,
        n_register_ok: 0,
        n_register_fail: 0,
        n_dispatch_hit: 0,
        n_dispatch_miss: 0,
        compile_nanos: 0,
    };
    let mut store = Store::new(&engine, state);

    let instance = linker.instantiate(&mut store, &module)?;
    // Cache the guest's exported memory before _start runs (register_block
    // needs it to read IR bytes and to bind the compiled block's import).
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().guest_memory = Some(mem);
    }
    let start = instance.get_typed_func::<(), ()>(&mut store, "_start")?;
    let run_result = start.call(&mut store, ());
    dump_stats(&store);
    match run_result {
        Ok(()) => Ok(()),
        Err(e) => {
            if let Some(exit) = e.downcast_ref::<wasmtime_wasi::I32Exit>() {
                std::process::exit(exit.0);
            }
            Err(e)
        }
    }
}

fn dump_stats(store: &Store<HostState>) {
    let st = store.data();
    eprintln!(
        "[jit-host] stats: blocks={} reg_ok={} reg_fail={} dispatch_hit={} dispatch_miss={} compile_ms={:.1}",
        st.blocks.len(),
        st.n_register_ok,
        st.n_register_fail,
        st.n_dispatch_hit,
        st.n_dispatch_miss,
        st.compile_nanos as f64 / 1e6,
    );
}

fn register_block_inner(
    caller: &mut Caller<'_, HostState>,
    pc: u64,
    end_pc: u64,
    ir_ptr: i32,
    ir_len: i32,
) -> Result<()> {
    let mem = caller
        .data()
        .guest_memory
        .ok_or_else(|| Error::msg("guest memory not cached"))?;
    let data = mem.data(&caller);
    let start = ir_ptr as usize;
    let end = start
        .checked_add(ir_len as usize)
        .ok_or_else(|| Error::msg("ir_ptr+len overflow"))?;
    if end > data.len() {
        return Err(Error::msg("ir buffer out of bounds"));
    }
    let ir_bytes = data[start..end].to_vec();

    // Debugging: optionally truncate IR to first N ops to bisect bad blocks.
    let ir_bytes = if let Ok(n) = std::env::var("JIT_TRUNCATE_OPS_AT_PC") {
        let parts: Vec<&str> = n.split('@').collect();
        if parts.len() == 2 {
            let trunc_pc =
                u64::from_str_radix(parts[1].trim_start_matches("0x"), 16).unwrap_or(0);
            let trunc_n: usize = parts[0].parse().unwrap_or(usize::MAX);
            if pc == trunc_pc {
                let new_len = (trunc_n * 16).min(ir_bytes.len());
                ir_bytes[..new_len].to_vec()
            } else {
                ir_bytes
            }
        } else {
            ir_bytes
        }
    } else {
        ir_bytes
    };
    let bytes = codegen::build_block(&ir_bytes, end_pc);
    if let Ok(idx_str) = std::env::var("JIT_DUMP_NTH") {
        if let Ok(idx) = idx_str.parse::<u64>() {
            let cur = caller.data().n_register_ok;
            if cur == idx {
                let path = format!("/tmp/jit_block_n{}_{:x}.wasm", idx, pc);
                let ir_path = format!("/tmp/jit_block_n{}_{:x}.ir.bin", idx, pc);
                let _ = std::fs::write(&path, &bytes);
                let _ = std::fs::write(&ir_path, &ir_bytes);
                eprintln!(
                    "[jit-host] dumped block #{idx} pc=0x{pc:x} end_pc=0x{end_pc:x} ir_len={ir_len} bytes_len={} -> {path}",
                    bytes.len()
                );
            }
        }
    }
    let engine = caller.engine().clone();
    let module = Module::from_binary(&engine, &bytes)?;
    let inst = Instance::new(&mut *caller, &module, &[Extern::Memory(mem)])?;
    let typed = inst
        .get_typed_func::<i32, i64>(&mut *caller, "block")
        .map_err(|e| Error::msg(format!("compiled module typed-func error: {e}")))?;
    caller.data_mut().blocks.insert(pc, Some(typed));
    Ok(())
}
