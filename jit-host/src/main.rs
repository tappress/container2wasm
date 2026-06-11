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

use jit_host::codegen;
use std::collections::{HashMap, VecDeque};
use wasmtime::*;
use wasmtime_wasi::WasiCtxBuilder;
use wasmtime_wasi::p1::WasiP1Ctx;

/// A compiled block in the content cache. `slot` is its index in the guest
/// module's exported funcref table (wasm-side dispatch builds, variant 4);
/// `None` on legacy host-dispatch builds (variants <= 3).
#[derive(Clone)]
struct CachedBlock {
    typed: TypedFunc<i32, i64>,
    slot: Option<u32>,
}

struct HostState {
    wasi: WasiP1Ctx,
    /// Compiled-block table keyed by guest PC. `Some(Some(_))` = compiled,
    /// `Some(None)` = "uncompilable" sentinel (don't retry). Only consulted
    /// by host-side dispatch (variants <= 3); on wasm-dispatch builds it
    /// holds just the uncompilable sentinels.
    blocks: HashMap<u64, Option<TypedFunc<i32, i64>>>,
    /// Content-addressed cache: (start_pc, end_pc, ir_bytes) -> compiled
    /// block. Survives flushes — a flush only drops pc->block mappings;
    /// re-registration after a rescan re-links from here without recompiling
    /// or re-instantiating. Staleness-immune by construction: the scanner
    /// always reads current guest bytes, so changed code produces a different
    /// key. start_pc is part of the key because memory ops bake absolute
    /// fault pcs derived from it. Also keeps the store's instance count
    /// bounded by unique block content (wasmtime caps instances per store at
    /// ~10k by default).
    module_cache: HashMap<(u64, u64, Vec<u8>), CachedBlock>,
    /// Cached handle to the guest module's exported linear memory.
    guest_memory: Option<Memory>,
    /// Memory helpers exported by the guest (c2w_jit_lb..sd, indexed by
    /// helper id — see codegen::MEM_HELPER_EXPORTS). Present on Batch 4+
    /// builds; blocks containing loads/stores import them at instantiation.
    mem_helpers: Option<Vec<Func>>,
    /// The guest module's exported funcref table (`-Wl,--export-table`).
    /// Present = wasm-side dispatch: register_block grows this table with the
    /// compiled block's Func and returns the slot index; the guest then calls
    /// blocks via call_indirect without ever crossing back into the host.
    guest_table: Option<Table>,
    /// TLB layout from the guest's `c2w_jit_tlb_layout` export, used only
    /// when JIT_INLINE_TLB=1 opts in (Batch 5 measured the inline probe as a
    /// net loss under wasmtime — see codegen::TlbLayout). None = helper-call
    /// mem codegen, the default.
    tlb_layout: Option<codegen::TlbLayout>,
    /// Raises wasmtime's default 10k-instances-per-store cap — every unique
    /// compiled block is an instance, and V8's self-modifying code mints new
    /// unique content continuously, so Node workloads blow past 10k.
    limits: StoreLimits,
    n_register_ok: u64,
    n_register_fail: u64,
    n_cache_hit: u64,
    n_dispatch_hit: u64,
    n_dispatch_miss: u64,
    /// Mapping-change flushes received, indexed by kind (satp/sfence/fence.i).
    n_flush: [u64; 3],
    compile_nanos: u64,
    /// Ring buffer of recent (pc_in, pc_out) dispatch pairs. Empty unless
    /// JIT_TRACE_DISPATCH was set. See dispatch_indirect import below.
    dispatch_trace: VecDeque<(u64, u64)>,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: {} <c2w.wasm> [guest args...]", args[0]);
        std::process::exit(2);
    }
    let wasm_path = &args[1];
    let guest_args = &args[2..];

    // JIT_TIMEOUT_SECS=N triggers wasmtime epoch interruption after N seconds —
    // turns a hang into a clean trap so dump_stats (and the dispatch trace) get
    // to run before we exit. Off by default.
    let timeout_secs: u64 = std::env::var("JIT_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut config = Config::new();
    if timeout_secs > 0 {
        config.epoch_interruption(true);
    }
    let engine = Engine::new(&config)?;
    if timeout_secs > 0 {
        let engine_clone = engine.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(timeout_secs));
            // Several increments — store.set_epoch_deadline(1) below means even
            // one is enough, but multiple guarantees the trap fires at the
            // next backedge regardless of where we are.
            for _ in 0..8 {
                engine_clone.increment_epoch();
            }
        });
    }

    // Pre-flight: build, compile, instantiate, and invoke a trivial dynamic
    // wasm module to verify the wasm-encoder -> wasmtime pipeline before we
    // depend on it during guest execution.
    {
        let bytes = codegen::build_preflight();
        let m = Module::from_binary(&engine, &bytes)?;
        let mut probe_store: Store<()> = Store::new(&engine, ());
        if timeout_secs > 0 {
            // epoch_interruption is on engine-wide; without an explicit
            // deadline here the probe would trap on the timer's first bump.
            probe_store.set_epoch_deadline(u64::MAX);
        }
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
    // JIT_TRACE_DISPATCH=N: log the LAST N (pc_in, pc_out) dispatch pairs to
    // stderr at exit. Stored in a ring buffer to avoid unbounded growth or
    // tearing apart hot loops with per-call I/O. N=0 disables.
    let trace_capacity: usize = std::env::var("JIT_TRACE_DISPATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    linker.func_wrap(
        "jit",
        "dispatch_indirect",
        move |mut caller: Caller<'_, HostState>, pc: i64, state_ptr: i32| -> Result<i64> {
            let typed = match caller.data().blocks.get(&(pc as u64)) {
                Some(Some(f)) => f.clone(),
                _ => {
                    caller.data_mut().n_dispatch_miss += 1;
                    return Ok(0);
                }
            };
            // Propagate any trap from the compiled block (incl. epoch deadline)
            // up to the c2w wasm caller and then out of start.call. Without
            // this, the previous `.unwrap_or(0)` swallowed traps and turned
            // them into "fallback to interpreter", which both hid bugs and
            // made JIT_TIMEOUT_SECS unable to escape JIT-tight loops.
            let r = typed.call(&mut caller, state_ptr)?;
            let st = caller.data_mut();
            st.n_dispatch_hit += 1;
            if trace_capacity > 0 {
                st.dispatch_trace.push_back((pc as u64, r as u64));
                while st.dispatch_trace.len() > trace_capacity {
                    st.dispatch_trace.pop_front();
                }
            }
            Ok(r)
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
            // Failure sentinel differs by dispatch mode: wasm-side dispatch
            // (guest table exported) treats any value <= 0 as failure, legacy
            // host dispatch checks != 0.
            let fail = if caller.data().guest_table.is_some() { -1 } else { 0 };
            if disable_compile
                || caller.data().n_register_ok >= max_blocks
                || skip_pcs.contains(&(pc as u64))
            {
                caller.data_mut().blocks.insert(pc as u64, None);
                return fail;
            }
            let t0 = std::time::Instant::now();
            let r = match register_block_inner(&mut caller, pc as u64, end_pc as u64, ir_ptr, ir_len) {
                Ok(ret) => {
                    caller.data_mut().n_register_ok += 1;
                    ret
                }
                Err(e) => {
                    if std::env::var_os("JIT_LOG_FAIL").is_some() {
                        eprintln!("[jit-host] register_block failed @ pc=0x{:x}: {e}", pc);
                    }
                    let st = caller.data_mut();
                    st.blocks.insert(pc as u64, None);
                    st.n_register_fail += 1;
                    fail
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
    // The guest signals "VA->code mapping may have changed". kind 0 = satp
    // write: only user-half blocks can be stale (kernel half is globally
    // mapped), so retain those. kind 1 = sfence.vma: addr != 0 limits the
    // drop to that page (blocks never span pages). kind 2 = fence.i: drop
    // everything. JIT_IGNORE_FLUSH=1 turns this into a counter-only no-op so
    // the same wasm can be A/B-run with invalidation off.
    //
    // On wasm-dispatch builds (guest table exported) this is stats-only: the
    // guest invalidates its own pc->slot map via generation counters, and the
    // host's `blocks` map isn't consulted for dispatch.
    let ignore_flush = std::env::var_os("JIT_IGNORE_FLUSH").is_some();
    const KERNEL_HALF_BASE: u64 = 0xffff_ffc0_0000_0000;
    const PG_MASK: u64 = 0xfff;
    linker.func_wrap(
        "jit",
        "flush_blocks",
        move |mut caller: Caller<'_, HostState>, kind: i32, addr: i64| {
            let wasm_dispatch = caller.data().guest_table.is_some();
            let st = caller.data_mut();
            st.n_flush[(kind as usize).min(2)] += 1;
            if ignore_flush || wasm_dispatch {
                return;
            }
            match kind {
                0 => st.blocks.retain(|&pc, _| pc >= KERNEL_HALF_BASE),
                1 if addr != 0 => {
                    let page = (addr as u64) & !PG_MASK;
                    st.blocks.retain(|&pc, _| pc & !PG_MASK != page);
                }
                _ => st.blocks.clear(),
            }
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
        module_cache: HashMap::new(),
        limits: StoreLimitsBuilder::new()
            .instances(1_000_000)
            .build(),
        guest_memory: None,
        mem_helpers: None,
        guest_table: None,
        tlb_layout: None,
        n_register_ok: 0,
        n_register_fail: 0,
        n_cache_hit: 0,
        n_dispatch_hit: 0,
        n_dispatch_miss: 0,
        n_flush: [0; 3],
        compile_nanos: 0,
        dispatch_trace: VecDeque::new(),
    };
    let mut store = Store::new(&engine, state);
    store.limiter(|s| &mut s.limits);
    if timeout_secs > 0 {
        store.set_epoch_deadline(1);
    }

    let instance = linker.instantiate(&mut store, &module)?;
    // Cache the guest's exported memory before _start runs (register_block
    // needs it to read IR bytes and to bind the compiled block's import).
    if let Some(mem) = instance.get_memory(&mut store, "memory") {
        store.data_mut().guest_memory = Some(mem);
    }
    // Exported funcref table (-Wl,--export-table) selects wasm-side dispatch.
    if let Some(table) = instance.get_table(&mut store, "__indirect_function_table") {
        store.data_mut().guest_table = Some(table);
        eprintln!(
            "[jit-host] guest exports its funcref table (size {}): wasm-side dispatch",
            table.size(&store)
        );
    }
    // Memory helpers (Batch 4+ builds): all 11 or none. Without them, blocks
    // containing loads/stores fail registration and fall back to interpreting.
    let helpers: Option<Vec<Func>> = codegen::MEM_HELPER_EXPORTS
        .iter()
        .map(|n| instance.get_func(&mut store, n))
        .collect();
    if let Some(h) = helpers {
        store.data_mut().mem_helpers = Some(h);
        eprintln!("[jit-host] guest exports mem helpers: load/store codegen enabled");
    }
    // TLB layout (Batch 5+ builds): pure constant-returning export, safe to
    // call before _start. The query is selector-keyed; sanity-check the
    // power-of-two assumptions the codegen bakes in as shifts/masks.
    // Opt-in: same-day A/B on this box showed the inline probe losing to the
    // plain helper call (loop 70.9s vs 68.5s) — wasmtime's cross-module call
    // is a few ns, cheaper than duplicating the probe in every block.
    if std::env::var_os("JIT_INLINE_TLB").is_none() {
        // default: helper-call mem codegen
    } else if let Ok(q) =
        instance.get_typed_func::<i32, i32>(&mut store, "c2w_jit_tlb_layout")
    {
        let g = |store: &mut Store<HostState>, k: i32| -> Result<u32> {
            Ok(q.call(store, k)? as u32)
        };
        let tlb_size = g(&mut store, 2)?;
        let entry_size = g(&mut store, 3)?;
        if !tlb_size.is_power_of_two() || !entry_size.is_power_of_two() {
            return Err(Error::msg(format!(
                "c2w_jit_tlb_layout: TLB_SIZE {tlb_size} / sizeof(TLBEntry) {entry_size} not powers of two"
            )));
        }
        let layout = codegen::TlbLayout {
            tlb_read_off: g(&mut store, 0)?,
            tlb_write_off: g(&mut store, 1)?,
            idx_mask: tlb_size - 1,
            entry_shift: entry_size.trailing_zeros(),
            vaddr_off: g(&mut store, 4)?,
            addend_off: g(&mut store, 5)?,
            pg_shift: g(&mut store, 6)?,
        };
        eprintln!(
            "[jit-host] guest exports TLB layout (read@{} write@{} entries={} esz={} pg=2^{}): inline-TLB codegen enabled",
            layout.tlb_read_off, layout.tlb_write_off, tlb_size, entry_size, layout.pg_shift
        );
        store.data_mut().tlb_layout = Some(layout);
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
    let table_size = st.guest_table.map_or(0, |t| t.size(store));
    eprintln!(
        "[jit-host] stats: blocks={} cache={} table={} reg_ok={} cache_hit={} reg_fail={} dispatch_hit={} dispatch_miss={} flush_satp={} flush_sfence={} flush_fencei={} compile_ms={:.1}",
        st.blocks.len(),
        st.module_cache.len(),
        table_size,
        st.n_register_ok,
        st.n_cache_hit,
        st.n_register_fail,
        st.n_dispatch_hit,
        st.n_dispatch_miss,
        st.n_flush[0],
        st.n_flush[1],
        st.n_flush[2],
        st.compile_nanos as f64 / 1e6,
    );
    if !st.dispatch_trace.is_empty() {
        eprintln!("[jit-host] last {} dispatches (pc_in -> pc_out):", st.dispatch_trace.len());
        for (pc_in, pc_out) in st.dispatch_trace.iter() {
            eprintln!("  0x{pc_in:x} -> 0x{pc_out:x}");
        }
    }
}

/// Compiles + registers one block. Returns the value handed back to the
/// guest: the funcref-table slot index on wasm-dispatch builds, or 1 on
/// legacy host-dispatch builds.
fn register_block_inner(
    caller: &mut Caller<'_, HostState>,
    pc: u64,
    end_pc: u64,
    ir_ptr: i32,
    ir_len: i32,
) -> Result<i32> {
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

    // Debugging: env-var filters to reject specific op classes.
    // JIT_NO_BRANCH=1   skips blocks whose last op is a conditional branch.
    // JIT_NO_JUMP=1     skips blocks whose last op is JAL/JALR.
    // JIT_NO_MEM=1      skips blocks containing any load/store.
    if !ir_bytes.is_empty() {
        let last_kind = ir_bytes[ir_bytes.len() - 16];
        if std::env::var_os("JIT_NO_BRANCH").is_some() && (32..=37).contains(&last_kind) {
            return Err(Error::msg("JIT_NO_BRANCH: skipping branch terminator"));
        }
        if std::env::var_os("JIT_NO_JUMP").is_some() && (last_kind == 30 || last_kind == 31) {
            return Err(Error::msg("JIT_NO_JUMP: skipping jump terminator"));
        }
        if std::env::var_os("JIT_NO_MEM").is_some()
            && ir_bytes.chunks_exact(16).any(|c| (38..=48).contains(&c[0]))
        {
            return Err(Error::msg("JIT_NO_MEM: skipping block with memory ops"));
        }
    }

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
    // Content-cache hit: same IR bytes + same start/end pc produce
    // byte-identical wasm, so reuse the existing instance instead of
    // recompiling.
    let cache_key = (pc, end_pc, ir_bytes.clone());
    if let Some(cb) = caller.data().module_cache.get(&cache_key) {
        let cb = cb.clone();
        let st = caller.data_mut();
        st.n_cache_hit += 1;
        return Ok(match cb.slot {
            // Wasm-side dispatch: the block already sits in the guest table;
            // the guest re-inserts pc->slot into its own map.
            Some(slot) => slot as i32,
            None => {
                st.blocks.insert(pc, Some(cb.typed));
                1
            }
        });
    }
    let tlb = caller.data().tlb_layout;
    let (bytes, used_helpers) = codegen::build_block(&ir_bytes, pc, end_pc, tlb.as_ref());
    // JIT_DUMP_PCS=11eb0,ffffffff8002561a,... — dump IR+wasm for specific PCs
    // (hex, no 0x prefix needed) at registration time.
    if let Ok(pcs_str) = std::env::var("JIT_DUMP_PCS") {
        let wanted = pcs_str
            .split(',')
            .filter_map(|t| u64::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok())
            .any(|p| p == pc);
        if wanted {
            let path = format!("/tmp/jit_block_pc{pc:x}.wasm");
            let ir_path = format!("/tmp/jit_block_pc{pc:x}.ir.bin");
            let _ = std::fs::write(&path, &bytes);
            let _ = std::fs::write(&ir_path, &ir_bytes);
            eprintln!(
                "[jit-host] dumped block pc=0x{pc:x} end_pc=0x{end_pc:x} ir_len={} -> {path}",
                ir_bytes.len()
            );
        }
    }
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
    // Imports are positional: guest memory first, then the helpers the block
    // uses, in the order build_block declared them.
    let externs: Vec<Extern> = {
        let mut v = vec![Extern::Memory(mem)];
        if !used_helpers.is_empty() {
            let helpers = caller.data().mem_helpers.as_ref().ok_or_else(|| {
                Error::msg("block uses memory ops but guest exports no c2w_jit_* helpers")
            })?;
            v.extend(used_helpers.iter().map(|&h| Extern::Func(helpers[h])));
        }
        v
    };
    let inst = Instance::new(&mut *caller, &module, &externs)?;
    let typed = inst
        .get_typed_func::<i32, i64>(&mut *caller, "block")
        .map_err(|e| Error::msg(format!("compiled module typed-func error: {e}")))?;
    let (ret, slot) = if let Some(table) = caller.data().guest_table {
        // Wasm-side dispatch: append the block to the guest's funcref table.
        // grow() returns the previous size, i.e. the new slot's index. The
        // guest calls it via call_indirect, so the (i32)->(i64) type check
        // happens there — engine-wide type canonicalization makes the
        // cross-module Func match.
        let func = *typed.func();
        let slot = table
            .grow(&mut *caller, 1, Ref::Func(Some(func)))
            .map_err(|e| Error::msg(format!("table.grow failed: {e}")))?;
        let slot = u32::try_from(slot).map_err(|_| Error::msg("table slot > u32"))?;
        (slot as i32, Some(slot))
    } else {
        caller.data_mut().blocks.insert(pc, Some(typed.clone()));
        (1, None)
    };
    caller
        .data_mut()
        .module_cache
        .insert(cache_key, CachedBlock { typed, slot });
    Ok(ret)
}
