importScripts("https://cdn.jsdelivr.net/npm/xterm-pty@0.9.4/workerTools.js");
importScripts(location.origin + "/browser_wasi_shim/index.js");
importScripts(location.origin + "/browser_wasi_shim/wasi_defs.js");
importScripts(location.origin + "/worker-util.js");
importScripts(location.origin + "/wasi-util.js");

// ---- URL params (worker is created as "./worker.js" + location.search) ----
function getParam(name) {
    var vars = location.search.substring(1).split('&');
    for (var i = 0; i < vars.length; i++) {
        var kv = vars[i].split('=');
        if (decodeURIComponent(kv[0]) == name) {
            return decodeURIComponent(kv.slice(1).join('='));
        }
    }
    return null;
}

// Container command override: ?cmd=echo%20hi (space-split) or
// ?cmd64=<base64 of JSON array> for args containing spaces/quotes.
function getCmdArgs() {
    var c64 = getParam('cmd64');
    if (c64) {
        return JSON.parse(atob(c64));
    }
    var c = getParam('cmd');
    if (c) {
        return c.split(' ').filter(function(s) { return s.length > 0; });
    }
    return [];
}

// ---- guest output capture (tail ring, for correctness checks + stats dump) ----
var outTail = [];
var OUT_TAIL_MAX = 32768;
function captureOut(buf) {
    for (var i = 0; i < buf.length; i++) outTail.push(buf[i]);
    if (outTail.length > OUT_TAIL_MAX) outTail = outTail.slice(outTail.length - OUT_TAIL_MAX);
}
function outTailString() {
    var s = '';
    for (var i = 0; i < outTail.length; i++) s += String.fromCharCode(outTail[i]);
    return s;
}

// ---- JIT coordinator -------------------------------------------------------
// Mirrors jit-host/src/main.rs register_block_inner: read IR from guest
// memory, content-cache on (pc, end_pc, n_insns, ir bytes), codegen via the
// jit_codegen wasm module (same codegen.rs as the wasmtime host), sync
// WebAssembly.Module + Instance against the guest's memory/table/helpers,
// table.grow + table.set, return the slot. Failure returns -1 (the guest
// marks the pc uncompilable). flush/mark are stats-only: invalidation is
// generation-checked guest-side.
var HELPER_NAMES = [
    "c2w_jit_lb", "c2w_jit_lh", "c2w_jit_lw", "c2w_jit_ld",
    "c2w_jit_lbu", "c2w_jit_lhu", "c2w_jit_lwu",
    "c2w_jit_sb", "c2w_jit_sh", "c2w_jit_sw", "c2w_jit_sd",
    "c2w_jit_amo_w", "c2w_jit_amo_d",
];

function makeJitState() {
    return {
        enabled: getParam('jit') !== 'off',
        chain: getParam('chain') === 'on',
        tlb: getParam('tlb') === 'on',
        lift: getParam('lift') === 'on',
        jcg: null,        // codegen wasm exports
        guest: null,      // guest instance exports, set after instantiate
        cache: new Map(), // content cache: key -> table slot
        guestImports: null,
        stats: {
            compiles: 0, cacheHits: 0, regFail: 0, uncompilable: 0,
            flushSatp: 0, flushSfence: 0, flushFencei: 0,
            compileMs: 0, codegenMs: 0, moduleMs: 0,
        },
    };
}

function irKey(pc, end_pc, n_insns, mem, ptr, len) {
    var key = pc + ':' + end_pc + ':' + n_insns + ':';
    var v = new Uint8Array(mem, ptr, len);
    for (var i = 0; i < len; i += 4096) {
        key += String.fromCharCode.apply(null, v.subarray(i, Math.min(i + 4096, len)));
    }
    return key;
}

function makeJitImports(js) {
    var s = js.stats;
    return {
        register_block2: function(pc, end_pc, ir_ptr, ir_len, n_insns) {
            if (!js.enabled || !js.guest) return -1;
            var tA = performance.now();
            try {
                var g = js.guest;
                var key = irKey(pc, end_pc, n_insns, g.memory.buffer, ir_ptr, ir_len);
                var hit = js.cache.get(key);
                if (hit !== undefined) { s.cacheHits++; return hit; }
                var jcg = js.jcg;
                var inPtr = jcg.jcg_in_ptr(ir_len);
                new Uint8Array(jcg.memory.buffer, inPtr, ir_len)
                    .set(new Uint8Array(g.memory.buffer, ir_ptr, ir_len));
                var tC = performance.now();
                var outLen = jcg.jcg_build(ir_len, pc, end_pc, n_insns);
                var bytes = new Uint8Array(jcg.memory.buffer, jcg.jcg_out_ptr(), outLen);
                s.codegenMs += performance.now() - tC;
                if (!js.guestImports) {
                    var gi = { mem: g.memory, table: g.__indirect_function_table };
                    for (var i = 0; i < HELPER_NAMES.length; i++) {
                        if (g[HELPER_NAMES[i]]) gi[HELPER_NAMES[i]] = g[HELPER_NAMES[i]];
                    }
                    js.guestImports = { guest: gi };
                }
                var tM = performance.now();
                var mod = new WebAssembly.Module(bytes);
                var inst = new WebAssembly.Instance(mod, js.guestImports);
                s.moduleMs += performance.now() - tM;
                var table = g.__indirect_function_table;
                var slot = table.grow(1);
                table.set(slot, inst.exports.block);
                js.cache.set(key, slot);
                s.compiles++;
                s.compileMs += performance.now() - tA;
                return slot;
            } catch (e) {
                s.regFail++;
                if (s.regFail <= 5) console.log('C2W_JIT_FAIL pc=0x' + pc.toString(16) + ' ' + e);
                return -1;
            }
        },
        mark_uncompilable: function(pc) { s.uncompilable++; },
        flush_blocks: function(kind, addr) {
            if (kind === 0) s.flushSatp++;
            else if (kind === 1) s.flushSfence++;
            else s.flushFencei++;
        },
    };
}

// Chain / inline-TLB opt-in: query the guest's layout export (pure constant
// reads, safe pre-_start) and hand the layouts to the codegen module — same
// selector protocol as jit-host/src/main.rs.
function configureLayouts(js) {
    if (js.lift && js.jcg.jcg_set_lift) {
        js.jcg.jcg_set_lift(1);
        console.log('C2W_JIT register lifting enabled');
    }
    var q = js.guest.c2w_jit_tlb_layout;
    if (!q) return;
    if (js.tlb) {
        var tlbSize = q(2) >>> 0, esz = q(3) >>> 0;
        js.jcg.jcg_set_tlb(q(0), q(1), tlbSize - 1, Math.log2(esz), q(4), q(5), q(6));
        console.log('C2W_JIT inline-TLB codegen enabled');
    }
    if (js.chain) {
        var vals = [];
        for (var i = 7; i <= 19; i++) {
            var v = q(i) >>> 0;
            if (v === 0xffffffff) { console.log('C2W_JIT chain layout absent'); return; }
            vals.push(v);
        }
        js.jcg.jcg_set_chain.apply(null, vals);
        if ((q(20) >>> 0) !== 1) throw 'chain selfcharge ack failed';
        js.chainHopsAddr = vals[12];
        console.log('C2W_JIT block chaining enabled (selfcharge acked)');
    }
}

// ---------------------------------------------------------------------------

onmessage = (msg) => {
    if (serveIfInitMsg(msg)) {
        return;
    }
    var ttyClient = new TtyClient(msg.data);
    var args = [];
    var env = [];
    var fds = [];
    var netParam = getNetParam();
    var listenfd = 3;
    run(ttyClient, args, env, fds, netParam, listenfd);
};

async function run(ttyClient, args, env, fds, netParam, listenfd) {
    var js = makeJitState();
    var result = { image: getImagename().split('/').pop(), jit: js.enabled, chain: js.chain, tlb: js.tlb };
    try {
        if (js.enabled) {
            var jr = await fetch(location.origin + '/jit_codegen.wasm');
            js.jcg = (await WebAssembly.instantiate(await jr.arrayBuffer(), {})).instance.exports;
        }
        var t0 = performance.now();
        var resp = await fetch(getImagename(), { credentials: 'same-origin' });
        var wasm = await resp.arrayBuffer();
        result.fetchMs = Math.round(performance.now() - t0);

        var cmd = getCmdArgs();
        args = ['arg0'];
        if (netParam && netParam.mode == 'delegate') {
            args = args.concat(['--net=socket', '--mac', genmac()]);
        }
        args = args.concat(cmd);
        result.args = args;

        var wasi = new WASI(args, env, fds);
        wasiHack(wasi, ttyClient, 5);
        wasiHackSocket(wasi, listenfd, 5);

        var t1 = performance.now();
        var imports = {
            wasi_snapshot_preview1: wasi.wasiImport,
            jit: makeJitImports(js),
            c2w_blk: { read: function() { return -1; }, write: function() { return -1; } },
        };
        var inst = (await WebAssembly.instantiate(wasm, imports)).instance;
        result.instantiateMs = Math.round(performance.now() - t1);
        js.guest = inst.exports;
        if (js.enabled) configureLayouts(js);

        var t2 = performance.now();
        var exitCode = null;
        try {
            wasi.start(inst);
            exitCode = 0;
        } catch (e) {
            var m = /exit with exit code (\d+)/.exec('' + e);
            if (m) {
                exitCode = parseInt(m[1]);
            } else {
                result.error = '' + e + (e && e.stack ? ' | ' + e.stack : '');
            }
        }
        result.runMs = Math.round(performance.now() - t2);
        result.exitCode = exitCode;
    } catch (e) {
        result.error = '' + e + (e && e.stack ? ' | ' + e.stack : '');
    }
    var s = js.stats;
    result.compiles = s.compiles;
    result.cacheHits = s.cacheHits;
    result.regFail = s.regFail;
    result.uncompilable = s.uncompilable;
    result.flush = [s.flushSatp, s.flushSfence, s.flushFencei];
    result.compileMs = Math.round(s.compileMs);
    result.codegenMs = Math.round(s.codegenMs);
    result.moduleMs = Math.round(s.moduleMs);
    result.tableSize = js.guest && js.guest.__indirect_function_table ? js.guest.__indirect_function_table.length : 0;
    if (js.chainHopsAddr && js.guest) {
        try {
            result.chainHops = new DataView(js.guest.memory.buffer)
                .getBigUint64(js.chainHopsAddr, true).toString();
        } catch (e) {}
    }
    result.outputTail = outTailString().slice(-2048);
    console.log('C2W_RESULT ' + JSON.stringify(result));
    postMessage({ type: 'c2w-result', result: result });
}

// wasiHack patches wasi object for integrating it to xterm-pty.
function wasiHack(wasi, ttyClient, connfd) {
    // definition from wasi-libc https://github.com/WebAssembly/wasi-libc/blob/wasi-sdk-19/expected/wasm32-wasi/predefined-macros.txt
    const ERRNO_INVAL = 28;
    const ERRNO_AGAIN= 6;
    var _fd_read = wasi.wasiImport.fd_read;
    wasi.wasiImport.fd_read = (fd, iovs_ptr, iovs_len, nread_ptr) => {
        if (fd == 0) {
            var buffer = new DataView(wasi.inst.exports.memory.buffer);
            var buffer8 = new Uint8Array(wasi.inst.exports.memory.buffer);
            var iovecs = Iovec.read_bytes_array(buffer, iovs_ptr, iovs_len);
            var nread = 0;
            for (i = 0; i < iovecs.length; i++) {
                var iovec = iovecs[i];
                if (iovec.buf_len == 0) {
                    continue;
                }
                var data = ttyClient.onRead(iovec.buf_len);
                buffer8.set(data, iovec.buf);
                nread += data.length;
            }
            buffer.setUint32(nread_ptr, nread, true);
            return 0;
        } else {
            console.log("fd_read: unknown fd " + fd);
            return _fd_read.apply(wasi.wasiImport, [fd, iovs_ptr, iovs_len, nread_ptr]);
        }
        return ERRNO_INVAL;
    }
    var _fd_write = wasi.wasiImport.fd_write;
    wasi.wasiImport.fd_write = (fd, iovs_ptr, iovs_len, nwritten_ptr) => {
        if ((fd == 1) || (fd == 2)) {
            var buffer = new DataView(wasi.inst.exports.memory.buffer);
            var buffer8 = new Uint8Array(wasi.inst.exports.memory.buffer);
            var iovecs = Ciovec.read_bytes_array(buffer, iovs_ptr, iovs_len);
            var wtotal = 0
            for (i = 0; i < iovecs.length; i++) {
                var iovec = iovecs[i];
                var buf = buffer8.slice(iovec.buf, iovec.buf + iovec.buf_len);
                if (buf.length == 0) {
                    continue;
                }
                captureOut(buf);
                ttyClient.onWrite(Array.from(buf));
                wtotal += buf.length;
            }
            buffer.setUint32(nwritten_ptr, wtotal, true);
            return 0;
        } else {
            console.log("fd_write: unknown fd " + fd);
            return _fd_write.apply(wasi.wasiImport, [fd, iovs_ptr, iovs_len, nwritten_ptr]);
        }
        return ERRNO_INVAL;
    }
    wasi.wasiImport.poll_oneoff = (in_ptr, out_ptr, nsubscriptions, nevents_ptr) => {
        if (nsubscriptions == 0) {
            return ERRNO_INVAL;
        }
        let buffer = new DataView(wasi.inst.exports.memory.buffer);
        let in_ = Subscription.read_bytes_array(buffer, in_ptr, nsubscriptions);
        let isReadPollStdin = false;
        let isReadPollConn = false;
        let isClockPoll = false;
        let pollSubStdin;
        let pollSubConn;
        let clockSub;
        let timeout = Number.MAX_VALUE;
        for (let sub of in_) {
            if (sub.u.tag.variant == "fd_read") {
                if ((sub.u.data.fd != 0) && (sub.u.data.fd != connfd)) {
                    console.log("poll_oneoff: unknown fd " + sub.u.data.fd);
                    return ERRNO_INVAL; // only fd=0 and connfd is supported as of now (FIXME)
                }
                if (sub.u.data.fd == 0) {
                    isReadPollStdin = true;
                    pollSubStdin = sub;
                } else {
                    isReadPollConn = true;
                    pollSubConn = sub;
                }
            } else if (sub.u.tag.variant == "clock") {
                if (sub.u.data.timeout < timeout) {
                    timeout = sub.u.data.timeout
                    isClockPoll = true;
                    clockSub = sub;
                }
            } else {
                console.log("poll_oneoff: unknown variant " + sub.u.tag.variant);
                return ERRNO_INVAL; // FIXME
            }
        }
        let events = [];
        if (isReadPollStdin || isReadPollConn || isClockPoll) {
            var readable = false;
            if (isReadPollStdin || (isClockPoll && timeout > 0)) {
                readable = ttyClient.onWaitForReadable(timeout / 1000000000);
            }
            if (readable && isReadPollStdin) {
                let event = new Event();
                event.userdata = pollSubStdin.userdata;
                event.error = 0;
                event.type = new EventType("fd_read");
                events.push(event);
            }
            if (isReadPollConn) {
                var sockreadable = sockWaitForReadable();
                if (sockreadable == errStatus) {
                    return ERRNO_INVAL;
                } else if (sockreadable == true) {
                    let event = new Event();
                    event.userdata = pollSubConn.userdata;
                    event.error = 0;
                    event.type = new EventType("fd_read");
                    events.push(event);
                }
            }
            if (isClockPoll) {
                let event = new Event();
                event.userdata = clockSub.userdata;
                event.error = 0;
                event.type = new EventType("clock");
                events.push(event);
            }
        }
        var len = events.length;
        Event.write_bytes_array(buffer, out_ptr, events);
        buffer.setUint32(nevents_ptr, len, true);
        return 0;
    }
}

function getNetParam() {
    var vars = location.search.substring(1).split('&');
    for (var i = 0; i < vars.length; i++) {
        var kv = vars[i].split('=');
        if (decodeURIComponent(kv[0]) == 'net') {
            return {
                mode: kv[1],
                param: kv[2],
            };
        }
    }
    return null;
}

function genmac(){
    return "02:XX:XX:XX:XX:XX".replace(/X/g, function() {
        return "0123456789ABCDEF".charAt(Math.floor(Math.random() * 16))
    });
}
