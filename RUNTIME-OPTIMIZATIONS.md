# Runtime Optimizations: Bypassing TCG Emulation

This document describes performance optimizations that bypass QEMU's TCG emulation for hot paths using paravirtualization techniques.

## Overview

The kvmclock implementation proves that MSR-based hypercalls can bypass TCG emulation. These optimizations extend that pattern to other expensive operations.

**Key insight**: Each TCG instruction costs ~1-2 browser JS operations. Bypassing even 100 TCG instructions per operation provides measurable gains.

---

## Implemented Optimizations

### 1. 9p + Snapshot Restore Fix (Tier 1.3)

**Status**: ✅ Implemented (Jan 2026)

**Problem**: After snapshot restore, virtio-9p mounts failed because the backend state was stale (connected to build-time host, not browser MEMFS).

**Solution**: Mount retry loop with driver rebind as recovery mechanism.

**Key insight**: The virtio-9p backend needs time to reconnect to browser MEMFS after snapshot restore. The mount loop retries and attempts driver rebind after 10 failures.

**Implementation** (`cmd/init/main.go`):

1. **Mount retry loop** - Tries up to 50 times with 100ms delay
2. **Driver rebind** - After 10 failures, unbind/rebind 9pnet_virtio driver:
   ```go
   // Remount sysfs rw, unbind device, rebind device
   os.WriteFile("/sys/bus/virtio/drivers/9pnet_virtio/unbind", []byte(deviceName), 0200)
   os.WriteFile("/sys/bus/virtio/drivers/9pnet_virtio/bind", []byte(deviceName), 0200)
   ```
3. **Container bind mounts** - Add /mnt/wasi0 and /mnt/wasi1 to container spec so 9p paths are visible inside the container

**Result**: 9p now works with snapshot restore! Browser → Guest file access is functional.

**Limitations**:
- Write operations from guest to browser MEMFS don't work (returns "No error information")
- msize=512KB added but provides no benefit for small files (adds slight overhead)

**Benchmark Results (Battery Saver Mode):**

| Test | Previous (no 9p) | With 9p+snapshot |
|------|------------------|------------------|
| Boot to shell | 21s | 42s |
| CPU (10k loops) | 3.04s | 3.24s |
| I/O (100 reads) | 3.94s | 5.04s |

Boot is slower due to sysfs wait + rebind. I/O slightly slower but 9p now works.

**Expected gain**: Runtime configuration via /pack/info file now works with fast boot.

---

### 2. Serial Console Fastpath (Tier 1.1)

**Status**: ✅ Implemented

**Problem**: Each byte to serial port goes through UART state machine emulation (~100 TCG instructions/byte).

**Current flow**:
```
Guest write 0x3f8 → helper_outb() → serial_ioport_write() → UART FIFO → serial_xmit() → chardev
                    ↑ 100+ TCG instructions for state machine
```

**Optimized flow**:
```
Guest write 0x3f8 → helper_outb() → emscripten_serial_putchar() → browser TTY
                                   ↓ (also continues to normal path for logging)
                    → serial_ioport_write() → ...
```

**File**: `/home/and/Projects/qemu-wasm/target/i386/tcg/sysemu/misc_helper.c`

**Implementation**:
```c
#ifdef __EMSCRIPTEN__
#include <emscripten.h>

EM_JS(void, emscripten_serial_putchar, (int c), {
    if (typeof Module !== 'undefined' && Module['pty'] && Module['pty'].writable) {
        Module['pty'].write(new Uint8Array([c]));
    }
});
#endif

void helper_outb(CPUX86State *env, uint32_t port, uint32_t data)
{
#ifdef __EMSCRIPTEN__
    if (port == 0x3f8) {  // COM1 data port
        emscripten_serial_putchar(data & 0xff);
    }
#endif
    address_space_stb(&address_space_io, port, data,
                      cpu_get_mem_attrs(env), NULL);
}
```

**Expected gain**: 10-50ms faster boot (console output during early boot is faster).

---

### 3. PV Idle / HLT Handler (Tier 1.2)

**Status**: ✅ Implemented

**Problem**: HLT instruction causes QEMU to busy-poll, wasting 100% CPU when guest is idle.

**Current flow**:
```
Guest HLT → do_hlt() → cpu_loop_exit() → qemu_wait_io_event() → qemu_cond_wait()
                                                                 ↑ busy-polls in browser
```

**Optimized flow**:
```
Guest HLT → ... → qemu_wait_io_event() → emscripten_sleep(1) → yields to browser
                                        → qemu_cond_wait() (if still idle)
```

**File**: `/home/and/Projects/qemu-wasm/system/cpus.c`

**Implementation**:
```c
#ifdef __EMSCRIPTEN__
#include <emscripten.h>
#endif

void qemu_wait_io_event(CPUState *cpu)
{
    bool slept = false;

    while (cpu_thread_is_idle(cpu)) {
        if (!slept) {
            slept = true;
            qemu_plugin_vcpu_idle_cb(cpu);
        }
#ifdef __EMSCRIPTEN__
        qemu_mutex_unlock(&qemu_global_mutex);
        emscripten_sleep(1);  // Yield to browser event loop
        qemu_mutex_lock(&qemu_global_mutex);
        if (!cpu_thread_is_idle(cpu)) {
            break;
        }
#endif
        qemu_cond_wait(cpu->halt_cond, &qemu_global_mutex);
    }
    // ...
}
```

**Expected gain**: 50-90% reduction in idle CPU usage.

---

## Future Optimizations (Not Yet Implemented)

### Tier 2: Medium Effort, High Impact

| Optimization | Current Cost | Expected Gain | Effort |
|--------------|--------------|---------------|--------|
| clock_gettime fastpath | 300 TCG inst/call | 100-300ms boot | MEDIUM |
| read/write MSR hypercalls | 262ms/file op | 10-30% I/O | MEDIUM |
| virtio-console (hvc0) | UART state machine | 5-10x console speed | MEDIUM |

### Tier 3: High Effort, High Impact

| Optimization | Current Cost | Expected Gain | Effort |
|--------------|--------------|---------------|--------|
| Syscall interception | Full kernel emulation | 20-50% boot | HIGH |
| 9P handler in JS | QEMU virtio overhead | 50-100% I/O | VERY HIGH |
| fork/exec acceleration | 50-200ms each | 5-10s boot | VERY HIGH |

---

## MSR-Based Hypercall Framework

The kvmclock implementation provides a template for all hypercalls:

### Reserved MSR Range
```
0x4b564d00 - MSR_KVM_WALL_CLOCK_NEW (used by kvmclock)
0x4b564d01 - MSR_KVM_SYSTEM_TIME_NEW (used by kvmclock)
0x4b564d09 - 0x4b564dff - Available for custom hypercalls (~247 slots)
```

### Proposed Hypercall MSRs (Future)
```c
#define MSR_KVM_PV_READ      0x4b564d10  // Fast file read
#define MSR_KVM_PV_WRITE     0x4b564d11  // Fast file write
#define MSR_KVM_PV_CONSOLE   0x4b564d14  // Direct console output
#define MSR_KVM_PV_IDLE      0x4b564d15  // Idle notification
```

---

## Testing the Optimizations

### Testing 9p msize (Immediate)

The msize change is already in `examples/arg-module.js`. Test with:

```bash
# In browser console after boot:
time for i in $(seq 1 100); do cat /etc/passwd > /dev/null; done
```

### Testing QEMU Changes (Requires Rebuild)

```bash
# Option 1: Use local qemu-wasm with docker buildx
docker buildx build \
    --build-context qemu-repo=/home/and/Projects/qemu-wasm \
    --build-arg QEMU_MIGRATION=true \
    --target js-qemu-amd64 \
    -o type=local,dest=examples/ \
    .

# Option 2: Push qemu-wasm changes and use c2w
cd /home/and/Projects/qemu-wasm && git push
./out/c2w --to-js \
    --assets . \
    --build-arg QEMU_REPO=https://github.com/tappress/qemu-wasm \
    --build-arg QEMU_REPO_VERSION=master \
    --build-arg QEMU_MIGRATION=true \
    alpine:3.20 examples/

# Test in browser
cd examples && python3 serve.py
# Open http://localhost:8080/debug.html
```

### Measured Results (Jan 2026)

**Test configuration**: 1 vCPU, 128MB RAM, snapshot restore enabled

#### Power Saver Mode (~2.4 GHz, consistent results)

| Metric | Result | Per-operation |
|--------|--------|---------------|
| Boot to shell (snapshot) | **21 seconds** | - |
| 10k loop iterations | **3.04s** | 0.304ms/iter |
| 100 file reads | **3.94s** | 39.4ms/read |

#### Full Performance Mode (for comparison)

| Metric | Result | Per-operation |
|--------|--------|---------------|
| Boot to shell (snapshot) | **~9 seconds** | - |
| 10k loop iterations | **0.98s** | 0.098ms/iter |
| 100 file reads | **1.85s** | 18.5ms/read |

**Raw benchmark output (power saver)**:
```
# CPU test - 10,000 shell iterations
/ # time sh -c 'i=0; while [ $i -lt 10000 ]; do i=$((i+1)); done'
real    0m 3.04s
user    0m 2.51s
sys     0m 0.05s

# I/O test - 100 file reads
/ # time sh -c 'i=0; while [ $i -lt 100 ]; do cat /etc/passwd > /dev/null; i=$((i+1)); done'
real    0m 3.94s
user    0m 0.33s
sys     0m 2.69s
```

**Notes**:
- The massive improvement is primarily from snapshot restore (QEMU_MIGRATION=true)
- Serial fastpath and PV idle contribute to smoother operation
- Power saver mode is ~3x slower but provides consistent benchmarks

---

## Architecture Notes

### Why These Optimizations Work

1. **Serial fastpath**: EM_JS calls are ~1-5 browser operations vs ~100 TCG instructions
2. **PV idle**: `emscripten_sleep()` yields to browser instead of busy-polling
3. **9p msize**: Amortizes per-message overhead over larger chunks

### Limitations

- Serial fastpath doesn't check DLAB bit (assumes data writes, not divisor latch)
- PV idle uses fixed 1ms sleep (could be adaptive based on workload)
- These don't address the fundamental 37x TCG vs CheerpX JIT gap

### QEMU-wasm Commit

The QEMU changes are in commit `22c2f6a31e` on the `master` branch of `/home/and/Projects/qemu-wasm`:
```
perf: Add serial fastpath and PV idle for WASM builds
```
