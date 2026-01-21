# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**container2wasm** (c2w) is a container-to-WASM converter that enables running unmodified Linux containers on WebAssembly runtimes. It works by packaging a container image with an emulated CPU (Bochs, TinyEMU, or QEMU) and Linux kernel into a WASM binary.

### docker-wasm Fork Vision

This fork (**docker-wasm**) aims to enable full Docker functionality (daemon, CLI, build, compose) running inside browser-based Linux emulation - running `docker run`, `docker build`, `docker compose` directly in browser tabs without server infrastructure.

**Current Status**: Phase 1 - Docker-in-Docker PoC (VALIDATED)

### PoC Results (Jan 2026)

**What works:**
- ✅ containerd successfully boots in browser (takes ~330 seconds / 5.5 min)
- ✅ dockerd starts and attempts to initialize
- ✅ Linux kernel 6.1 with cgroups2 support runs in QEMU-wasm
- ✅ 1GB guest RAM configuration works (`--build-arg VM_MEMORY_SIZE_MB=1024`)

**Current blockers:**
- ❌ Loopback interface not configured - causes `bind: cannot assign requested address`
- ❌ dockerd times out waiting for containerd (default timeout < 330s boot time)
- ❌ Performance is very slow (~5 min for containerd to boot)

**Fix needed for loopback:**
```bash
ip link set lo up
ip addr add 127.0.0.1/8 dev lo
```

**DinD Dockerfile that works:**
```dockerfile
FROM alpine:3.20
RUN apk add --no-cache docker containerd runc iptables pigz e2fsprogs xz
RUN mkdir -p /var/lib/docker /run/docker
CMD ["dockerd", "--storage-driver=vfs", "--iptables=false"]
```

**Build command:**
```bash
c2w --to-js --build-arg VM_MEMORY_SIZE_MB=1024 dind-minimal /tmp/dind-browser/
```

### Performance Problem

The core issue is **emulation overhead**, not memory:
```
Browser JS → WASM runtime → QEMU (TCG JIT) → Linux → Docker
                              ↑
                    THIS IS THE BOTTLENECK
```

CheerpX is much faster because it has a purpose-built x86→WASM JIT, while QEMU's TCG was designed for native targets.

### Threading Investigation (Jan 2026)

**VERIFIED: MTTCG provides real multi-core parallelism!**

#### Benchmark Results (4 parallel processes × 10k iterations each)

| Config | Web Workers | Wall Clock | Speedup |
|--------|-------------|------------|---------|
| 1 vCPU | 4 (base pool) | 86.8s | 1x |
| 4 vCPU | 7 (+3 for vCPUs) | 16.1s | **5.4x** |

#### How it works

```
# Build with multi-core:
c2w --to-js --build-arg VM_CORE_NUMS=4 alpine:3.20 /tmp/out/

# QEMU args generated:
-smp 4,sockets=4 -accel tcg,tb-size=500,thread=multi
```

| System | Purpose | Implementation | Parallelism |
|--------|---------|----------------|-------------|
| Fiber coroutines | Async I/O (disk, network) | `--with-coroutine=fiber` | Single-threaded, cooperative |
| MTTCG | vCPU execution | pthreads → Web Workers | **TRUE parallel** |
| Emscripten pthread pool | Base threading | 4 workers (hardcoded) | Infrastructure |

**Key insights:**
- Fiber coroutines and MTTCG are **independent systems** - fibers handle I/O, MTTCG handles vCPU threads
- Emscripten creates 4 base workers + additional workers for each vCPU
- MTTCG provides **real parallelism** - 4 cores = ~5x speedup on parallel workloads

**Important clarification:** We initially thought `--with-coroutine=fiber` would block multi-threading. It doesn't! The build uses BOTH:
```
QEMU-wasm build (Dockerfile):
├── --with-coroutine=fiber     ← For async I/O (disk, network) - single-threaded
├── -pthread                    ← Enables Web Workers support
├── -sPROXY_TO_PTHREAD         ← Offloads main thread
└── thread=multi (runtime)      ← MTTCG creates vCPU threads via pthreads

Fiber coroutines ≠ Web Workers. They coexist independently.
No Dockerfile changes needed for multi-core support!
```

#### Performance comparison with CheerpX

| Test (10k loop) | QEMU-wasm | CheerpX | Difference |
|-----------------|-----------|---------|------------|
| Single-threaded | 17.3s | 0.46s | **37x slower** |

**Conclusion:** Multi-core helps for parallel workloads, but the fundamental bottleneck is TCG emulation speed. CheerpX's purpose-built x86→WASM JIT is 37x faster on single-threaded work.

#### Recommended build for Docker-in-Docker
```bash
c2w --to-js --build-arg VM_CORE_NUMS=4 --build-arg VM_MEMORY_SIZE_MB=1024 <image> /tmp/out/
```

### Performance Profiling (Jan 2026)

#### Where the time goes

Chrome DevTools profiling reveals:

```
Main thread breakdown (12s trace):
├── Run microtasks (WASM execution): 1,467ms (82.5%)  ← TCG emulation
├── Commit (rendering):              191ms (10.8%)
├── Threading overhead:              ~157ms (8.8%)
│   ├── emscripten_futex_wake        12.9ms
│   ├── em_task_queue_execute        37.0ms
│   ├── call_with_ctx                35.2ms
│   ├── call_then_finish_task        34.7ms
│   └── _emscripten_check_mailbox    37.3ms
└── wasm-to-js boundary:             20.9ms (1.2%)  ← NOT the bottleneck
```

**Key finding:** JS↔WASM boundary is only 1.2% of time. The bottleneck is inside WASM execution itself (TCG emulation).

#### Benchmark: CPU vs I/O (4-core Alpine)

| Test | Command | Real Time | User | Sys | Rate |
|------|---------|-----------|------|-----|------|
| **Pure CPU** | 50k shell iterations | 86.6s | 86.3s | 0.3s | 1.7ms/iter |
| **I/O heavy** | 1k file reads (cat) | 262.7s | 24.6s | 241.6s | 262ms/read |
| **Memory** | 10MB dd write | 4.4s | 0.16s | 4.2s | 2.3 MB/s |

#### Two bottlenecks identified

**Bottleneck #1: TCG Emulation**
```
- 1.7ms per shell arithmetic iteration
- CheerpX does same in ~0.05ms (37x faster)
- This is the fundamental speed limit of QEMU's TCG on WASM
- Trace linking disabled due to browser constraints (see Dockerfile)
```

**Bottleneck #2: Virtio-fs I/O (WORSE than expected)**
```
- 262ms per file operation (fork+exec+cat+exit)
- 150x slower than pure CPU operations
- Syscall path: guest kernel → virtio → QEMU → WASM → JS → browser
- Each syscall crosses multiple abstraction layers
```

#### Why Docker-in-Docker boot is slow

containerd/dockerd startup involves:
- Loading dozens of binaries (exec syscalls = expensive)
- Reading hundreds of config files
- Creating directories/files for state
- Setting up cgroups (filesystem operations)

With 262ms per file operation, thousands of operations during boot = minutes of wait time.

#### Disabled optimizations

From Dockerfile analysis:
```
--disable-trace-linking  # Causes "out of bounds memory access" in browser
--disable-trace-linking  # Causes "too much recursion" in browser
```

Trace linking is a TCG optimization that chains translation blocks - disabled due to WASM/browser limitations.

#### CRNG Entropy Optimization (Jan 2026)

**Problem:** CRNG (Cryptographic Random Number Generator) init took ~249 kernel seconds due to slow entropy gathering in emulation.

**Solution:** virtio-rng with rng-builtin backend provides entropy from browser's `crypto.getRandomValues()`.

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| CRNG init time | ~249s | ~33s | **7.5x faster** |

**Implementation:**

1. **QEMU args** (in arg-module.js):
```javascript
"-object", "rng-builtin,id=rng0", "-device", "virtio-rng-pci,rng=rng0"
```

2. **Dockerfile changes** (for Alpine kernel):
```dockerfile
# Add virtio_rng to modprobe list (and 9p modules)
echo 'modprobe -a iso9660 virtio_blk virtio_net virtio_pci virtio_rng 9pnet 9pnet_virtio 9p overlay'

# Add hwrng feature to initramfs
RUN echo 'kernel/drivers/char/hw_random' > /etc/mkinitfs/features.d/hwrng.modules
RUN ... mkinitfs ... features="...virtio hwrng 9p"
```

**How it works:**
```
Browser crypto.getRandomValues() → QEMU rng-builtin backend → virtio-rng PCI device → Linux kernel hwrng → CRNG
```

#### Virtio-9p Fix (Jan 2026)

**Problem:** Host directory sharing via virtio-9p wasn't working - mount returned "no such device" even though virtio-9p PCI devices were present.

**Root cause:** Alpine's `linux-virt` kernel has 9p support as **modules**, but:
1. The modules weren't included in the initramfs
2. The modules weren't being loaded at boot

**Solution:** Add 9p modules to initramfs and load them at boot.

**Dockerfile changes:**
```dockerfile
# Add 9pnet_virtio to modprobe (line ~756)
echo 'modprobe -a ... 9pnet 9pnet_virtio 9p overlay'

# Create 9p feature file for mkinitfs
RUN printf 'kernel/net/9p\nkernel/fs/9p\n' > /etc/mkinitfs/features.d/9p.modules

# Include 9p in initramfs features
features="...virtio hwrng 9p"
```

**Verification:**
```bash
# Before fix - no driver bound:
virtio0: 0x0009 - no driver

# After fix - 9pnet_virtio driver bound:
virtio0: 0x0009 - 9pnet_virtio
```

This enables browser→guest runtime configuration via `/pack/info` file.

See `CLAUDE-DOCKER-WASM.md` for:
- Detailed I/O architecture (virtio-fs data path, why 150x slower)
- JIT architecture analysis (why CheerpX is 37x faster)
- Potential optimization approaches (wasm-opt, x86→WASM JIT ideas)
- Full roadmap and architecture diagrams

See `QEMU-WASM-CHANGES.md` for:
- kvmclock/pvclock implementation for TCG (enables vDSO clock_gettime)
- All modifications to the qemu-wasm fork

## Build Commands

```bash
# Build the main tools (c2w and c2w-net)
make

# Install to /usr/local/bin
sudo make install

# Build individual components
make c2w                    # Container-to-WASM converter CLI
make c2w-net                # Network stack helper for WASI
make c2w-net-proxy.wasm     # Browser-side network proxy (WASI target)
make imagemounter.wasm      # Dynamic image pulling helper (WASI target)

# Tidy all go.mod files across submodules
make vendor
```

## Testing

Tests run inside Docker containers using docker-compose:

```bash
# Run the full integration test suite (requires Docker)
make test

# Run with custom Go test flags
GO_TEST_FLAGS="-run TestWasmtime" make test

# Run benchmarks
make benchmark
```

The test suite (`tests/test.sh`) spins up a containerized test environment with Docker-in-Docker, a local registry, and runs integration tests against multiple WASM runtimes (wasmtime, wazero, wasmer, wasmedge, wamr) and browsers.

To run a specific test pattern:
```bash
GO_TEST_FLAGS="-run TestWasmtime/wasmtime-hello" make test
```

## Converting Containers

```bash
# Convert container to WASI (runs on wasmtime, wazero, etc.)
c2w ubuntu:22.04 out.wasm
wasmtime out.wasm uname -a

# Convert container to browser-runnable JS/WASM (uses QEMU-wasm with JIT)
c2w --to-js alpine:3.20 /tmp/out/htdocs/

# Convert for different architectures
c2w --target-arch=riscv64 riscv64/ubuntu:22.04 out.wasm
c2w --target-arch=aarch64 arm64v8/alpine:3.20 out.wasm
```

## Architecture

### Key Components

- **`cmd/c2w/`**: Main CLI tool - orchestrates Docker buildx to create WASM images
- **`cmd/c2w-net/`**: Host-side network stack for WASI networking (uses gvisor-tap-vsock)
- **`cmd/init/`**: Init process that runs inside the emulated Linux guest
- **`extras/c2w-net-proxy/`**: Browser-side network proxy (compiled to WASI)
- **`extras/imagemounter/`**: Dynamic image pulling without pre-conversion
- **`Dockerfile`**: Multi-stage build that compiles emulators and packages everything

### Emulator Selection

| Flag | Emulator | Use Case |
|------|----------|----------|
| (default) | Bochs (x86_64) or TinyEMU (riscv64) | WASI runtimes |
| `--to-js` | QEMU-wasm | Browser with JIT compilation |

### Build Pipeline

The `c2w` command:
1. Pulls and saves the container image
2. Invokes `docker buildx build` with the embedded `Dockerfile`
3. The Dockerfile compiles the appropriate emulator (Bochs/TinyEMU/QEMU) to WASM
4. Packages the container filesystem, Linux kernel, and init system
5. Optionally pre-boots the kernel with wizer for faster startup

### Networking Modes

- **Browser Fetch API**: HTTP/HTTPS only, CORS restricted (`?net=browser`)
- **WebSocket delegation**: Full TCP via host proxy (`?net=delegate=ws://...`)
- **WASI sockets**: Native socket support with host-side c2w-net stack

### Memory Constraints

- WASM32 limit: ~4GB address space
- QEMU-wasm default: 2.3GB (`-sTOTAL_MEMORY=2300MB`)
- Guest VM default: 128MB (configurable via `VM_MEMORY_SIZE_MB` build arg)

## Code Organization

```
├── cmd/
│   ├── c2w/           # Main converter CLI
│   ├── c2w-net/       # Host network stack
│   ├── init/          # Guest init process
│   └── create-spec/   # OCI spec generator
├── extras/
│   ├── c2w-net-proxy/ # Browser network proxy
│   ├── imagemounter/  # Dynamic image loading
│   └── runcontainerjs/# Browser runtime helpers
├── examples/
│   ├── emscripten/    # Browser examples (QEMU-wasm)
│   ├── wasi-browser/  # Browser examples (Bochs)
│   └── networking/    # Network configuration examples
├── tests/
│   └── integration/   # Runtime tests (wasmtime, wazero, browsers)
├── Dockerfile         # Multi-stage build for WASM images
└── embed.go           # Embeds Dockerfile into c2w binary
```

## Development Notes

- Go 1.24+ required
- Docker 18.09+ with BuildKit required
- Docker Buildx v0.8+ recommended (falls back to legacy build if unavailable)
- The `Dockerfile` is embedded into the `c2w` binary via `embed.go`
- Multiple go.mod files exist for submodules (extras/, tests/) - use `make vendor` to tidy all

## QEMU-wasm Development Workflow

**Local fork location**: `/home/and/Projects/qemu-wasm` (tappress/qemu-wasm fork)

This is a fork of ktock/qemu-wasm with kvmclock/pvclock support for TCG. Changes here can be committed and pushed directly.

**Problem**: Docker rebuilds QEMU from scratch on source changes (~15-30 min with emscripten).

**Solution**: Build and test QEMU natively first - the TCG code is identical for native and WASM targets.

### Fast Iteration Cycle

```bash
# 1. Build native QEMU (first time ~5 min, incremental ~10-30 sec)
cd ~/Projects/qemu-wasm
mkdir build-native && cd build-native
../configure --target-list=x86_64-softmmu --enable-virtfs
make -j$(nproc)

# 2. After code changes, just run make (seconds)
make -j$(nproc)

# 3. Test with Alpine
./qemu-system-x86_64 -m 512 -nographic \
    -kernel /path/to/vmlinuz-lts \
    -initrd /path/to/initramfs-lts \
    -append "console=ttyS0"

# 4. Inside guest, verify changes:
dmesg | grep -i kvm
cat /sys/devices/system/clocksource/clocksource0/available_clocksource
```

### Why This Works

- TCG (Tiny Code Generator) backend code is **not WASM-specific**
- Same C code compiles for both native x86_64 host and WASM target
- Native builds support incremental compilation (only changed files rebuild)
- Can use gdb and other native debugging tools

### Workflow Summary

1. Edit code in `~/Projects/qemu-wasm`
2. `make` in `build-native` (seconds)
3. Test with native QEMU + Linux guest
4. Once working → build WASM version with c2w for browser test

### Building WASM Version with Local Changes

Use Docker buildx `--build-context` to override the qemu-repo stage:

```bash
# From docker-wasm directory
docker buildx build \
    --build-context qemu-repo=/home/and/Projects/qemu-wasm \
    --target js-qemu-amd64 \
    -t qemu-wasm-test \
    .
```

Or modify Dockerfile temporarily to use local COPY instead of git clone.

## Dockerfile Build Optimization

The Dockerfile uses BuildKit cache mounts for faster iterative builds.

### Cache Mounts Enabled

| Cache Type | Target | Stages Using It |
|------------|--------|-----------------|
| **apt** | `/var/cache/apt`, `/var/lib/apt` | All Ubuntu-based stages |
| **cargo** | `/usr/local/cargo/registry`, `target/` | wasi-vfs, wizer builds |
| **pip** | `/root/.cache/pip` | meson installs |
| **go** | `/root/.cache/go-build`, `/go/pkg/mod` | All Go builds |

### Build with Local Assets

To use local config files (kernel config, QEMU args templates) instead of cloning from GitHub:

```bash
# Use --assets flag to point to local directory
c2w --to-js --assets /home/and/Projects/docker-wasm alpine:3.20 /tmp/out/

# Or with custom QEMU repo
c2w --to-js \
    --assets /home/and/Projects/docker-wasm \
    --build-arg QEMU_REPO=https://github.com/tappress/qemu-wasm \
    --build-arg QEMU_REPO_VERSION=main \
    alpine:3.20 /tmp/out/
```

### Kernel Config Changes

When modifying kernel config (`config/qemu/linux_x86_config`):

```bash
# Rebuild with local assets to pick up config changes
c2w --to-js --assets . alpine:3.20 /tmp/out/

# Skip pre-boot optimization if kernel changes cause wizer to hang
c2w --to-js --assets . --build-arg QEMU_MIGRATION=false alpine:3.20 /tmp/out/
```

### Debugging Build Issues

```bash
# Build specific stage interactively
docker buildx build --target qemu-emscripten-dev-amd64 -t debug-qemu .
docker run -it debug-qemu bash

# Check which stages are being rebuilt
BUILDKIT_PROGRESS=plain docker buildx build --target js-qemu-amd64 . 2>&1 | grep -E "^#[0-9]+"
```

### Speeding Up Iterative Development

1. **Use native QEMU for code changes** (see section above) - seconds vs minutes
2. **Cache mounts persist between builds** - apt/cargo/go downloads only happen once
3. **Use `--assets` flag** - avoids re-cloning from GitHub
4. **Target specific stages** - `--target js-qemu-amd64` skips unneeded architectures

## Browser Debug Workflow

### Quick Start

```bash
# 1. Start the HTTP server with required headers (COOP/COEP for SharedArrayBuffer)
cd /home/and/Projects/docker-wasm/examples
python3 serve.py

# 2. Open debug console in browser
# http://localhost:8080/debug.html
```

### Debug Console Features

The `examples/debug.html` provides:

| Feature | Purpose |
|---------|---------|
| **Status panel** | Shows browser cores, VM cores, worker count, boot time |
| **Terminal** | xterm.js terminal with proper TTY poll implementation |
| **Console log** | Captures console.error/warn with timestamps |
| **Error tracking** | window.onerror handler for uncaught exceptions |

### Expected Boot Timeline

| Time | Event |
|------|-------|
| 0s | "QEMU starting..." |
| 1-2s | SeaBIOS splash in terminal |
| 5-10s | Linux kernel boot messages |
| 2-3min | Alpine login prompt (`localhost login:`) |

### Common Issues

| Issue | Cause | Solution |
|-------|-------|----------|
| Terminal goes blank after SeaBIOS | Missing `pty.readable` check in TTY poll | Use `debug.html` (has fix) |
| "SharedArrayBuffer is not defined" | Missing COOP/COEP headers | Use `serve.py` instead of basic HTTP server |
| Workers show 0 | Browser not supporting Web Workers | Use Chrome/Firefox with SharedArrayBuffer |
| Boot takes 5+ minutes | QEMU_MIGRATION not enabled | Build with `--build-arg QEMU_MIGRATION=true` (if working) |

### Building Fresh WASM for Testing

```bash
# Basic Alpine (1 vCPU, 128MB RAM) - fastest build
/home/and/Projects/docker-wasm/out/c2w \
    --to-js \
    alpine:3.20 \
    /home/and/Projects/docker-wasm/examples/

# Multi-core with more RAM (recommended for Docker-in-Docker)
/home/and/Projects/docker-wasm/out/c2w \
    --to-js \
    --build-arg VM_CORE_NUMS=4 \
    --build-arg VM_MEMORY_SIZE_MB=512 \
    --build-arg QEMU_MIGRATION=false \
    alpine:3.20 \
    /home/and/Projects/docker-wasm/examples/
```

### Files in examples/

| File | Size | Purpose |
|------|------|---------|
| `qemu-system-x86_64.wasm` | ~40MB | QEMU compiled to WebAssembly |
| `qemu-system-x86_64.data` | ~40MB | Packed filesystem (BIOS, kernel, rootfs) |
| `out.js` | ~227KB | Emscripten module loader |
| `load.js` | - | Data file loader |
| `arg-module.js` | - | QEMU arguments (vCPU count, RAM, TCG options) |
| `debug.html` | - | Debug console with diagnostics |
| `serve.py` | - | HTTP server with COOP/COEP headers |

### TTY Poll Fix (Critical)

The proper TTY poll implementation prevents terminal from disappearing:

```javascript
// Must check pty.readable before calling original poll
var oldPoll = Module['TTY'].stream_ops.poll;
var pty = Module['pty'];
Module['TTY'].stream_ops.poll = function(stream, timeout){
    if (!pty.readable) {
        return (pty.readable ? 1 : 0) | (pty.writable ? 4 : 0);
    }
    return oldPoll.call(stream, timeout);
}
```

This is already implemented in `debug.html` and `kvmclock-test.html`.
