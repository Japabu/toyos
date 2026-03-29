# ToyOS

A production-grade operating system built from scratch in Rust. Targets modern x86-64 hardware (2020+), UEFI only. ARM64 planned — keep architecture portable.

The name has no meaning. This is not a hobby project. The quality bar is the same as shipping software: correct, efficient, minimal, zero technical debt. The codebase is regularly scrutinized and refactored. Nothing is sacred except the principles below.

## Principles

- **Zero legacy.** No backwards compatibility. No fallbacks. No workarounds. No BIOS. No 32-bit. Research state-of-the-art OS design instead of replicating what older OSes do. We have no legacy to maintain — exploit that.
- **Zero technical debt.** Every feature is scrutinized. Dead code is deleted. Every abstraction earns its place.
- **Fail fast.** Panics over silent degradation. Exhaustive matches with panics for unexpected values. Never mask bugs. If something is unimplemented, the system screams and dies loudly.
- **Simplicity.** Prefer the simpler solution unless the complex one brings >2x improvement. We can simplify aggressively because we have no legacy constraints.
- **Rust is first class.** Not POSIX. Not C. The entire OS is Rust-native. C-isms tolerated only when the Rust alternative adds no safety or value.
- **Development ergonomics above all.** The ability to iterate fast matters more than feature count. Tooling comes first.
- **Self-hosting.** The north star is building ToyOS from within ToyOS. No LLVM dependency. Cranelift as codegen backend.
- **Efficient.** Never hog resources without purpose. Free memory when not used. Minimize kernel overhead. The OS must be fast and responsive. General improvements only — never optimize for one specific app.

## Architecture

> The architecture is under active development. Details here are a snapshot — always read the code for current state.

**Kernel** — Minimal. New additions to the kernel must be discussed and justified. Currently handles: resource management, scheduling, process lifecycle, filesystem, device arbitration.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel, maps hardware buffers into its own memory, and serves clients over IPC. Crash a daemon, the kernel is fine.

**IPC** — Named services via listen/accept/connect. Pipes backed by shared-memory ring buffers.

**Memory** — 2MB pages only. Demand paging. Shared memory across processes.

**Processes** — PIE binaries. Spawn-based. Full SMP. dlopen/dlsym for shared libraries.

**Scheduling** — Efficient, event-driven, fair-share. Per-CPU run queues. Must scale to 128+ cores without excessive overhead.

**Filesystem** — VFS with mount points. Initrd, tmpfs, NVMe.

**Syscall ABI** — Defined in `toyos-abi/`. The ABI is the contract between kernel and userland. Completely unstable — read the code for current state. Never add or change a syscall without discussion.

**Kernel must never crash from userland.** A buggy userland process must not be able to bring down the kernel. But if the kernel itself has a bug, it must crash loudly so we can fix it and harden.

## Dependencies

Only **Rust** and **QEMU** (for development). Everything else is bootstrapped from Rust:

- **toyos-ld** (`toyos-ld/`) — Custom linker. Used for bootloader, kernel, and all userland programs.
- **toyos-cc** (`toyos-cc/`) — Minimal C compiler. Not meant to grow — exists to bootstrap C compilers (tinycc) and compile doomgeneric.
- **rust/** — Fork of the Rust compiler and std with ToyOS platform support (submodule). Auto-bootstraps. Kept up to date with upstream.

## Ecosystem forks

Key crates forked in `userland/` with `[patch.crates-io]`. Rules:

- All changes must be upstream-mergeable. ToyOS aims to be a first-class platform in every important Rust ecosystem crate, alongside unix and windows.
- Use `#[cfg(target_os = "toyos")]` — never hijack existing cfg gates.
- Add ToyOS as a new platform alongside existing ones. Don't modify cross-platform code.

## Std library rules

- **Never edit cross-platform std files** (e.g. `library/std/src/process.rs`, `library/std/src/io/mod.rs`)
- Only edit ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`, files with `toyos` in the path
- Add ToyOS as a new platform alongside unix/windows/wasi — never hijack existing cfg gates

## Build & test

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU.
- `cargo test` runs integration tests (boots QEMU headless, runs test harness inside ToyOS).
- `system.toml` defines which programs to build and the init sequence.

## Repository layout

```
src/              Build system (the root cargo project)
kernel/           Kernel
bootloader/       UEFI bootloader
userland/         All userland programs + ecosystem forks
toyos-abi/        Syscall ABI (shared between kernel and userland)
toyos-ld/         Custom linker
toyos-cc/         Custom C compiler
toyos-net/        Networking library
rust/             Rust compiler/std fork (submodule)
tests/            Integration tests (QEMU-based)
system.toml       What to build and boot
```

## Debugging

**LLDB via QEMU** — All binaries are PIE, addresses change every boot. Parse serial output for `Kernel memory located at: 0x...` to load symbols with `--slide`. For userland, serial logs pid and base address at `spawn:`. Use `breakpoint set -r <pattern>` for Rust symbols (not `-n`, which doesn't work with `::` paths).

**Via test harness** (preferred): `cd tests && cargo test --test toyos_c -- --ignored debug --nocapture`, wait for `/tmp/toyos-debug-ready`, attach LLDB to `gdb-remote 1234`.

**Via full OS**: `cargo run` in background, attach LLDB to `gdb-remote 1234`. `--debug` flag pauses kernel before init via `DEBUG_WAIT` AtomicBool.

**QMP** (QEMU Machine Protocol) — Socket at `/tmp/toyos-qmp.sock`. Script at `.claude/qmp.py`:
- `python3 .claude/qmp.py "ls /bin"` — type string + Enter
- `python3 .claude/qmp.py --raw ret` — single key
- `python3 .claude/qmp.py --raw n --ctrl` — Ctrl+N
- `python3 .claude/qmp.py --screenshot /tmp/toyos-screen.png` — capture screen

## Workflow

- Stay focused on the current task. Write findings and issues into CLAUDE.md, don't go fix them — a separate agent will handle it.
- After each task, audit CLAUDE.md and update if the architecture or project state changed.
- If something is blocking, stop and report it. Don't work around it.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce output. If a command produces a lot of output or takes long, run it in the background — background tasks automatically get their output written to a file.

## Ideas

- **io_uring as the only blocking I/O mechanism.** Currently the kernel has two parallel notification paths: `scheduler::block(event)` for direct thread blocking and `io_uring::complete_pending_for_event` for ring watchers. Every wake site does both. If all fd-based blocking went through io_uring, blocking syscalls (read, write, accept) become non-blocking try-once-and-return, wake sites become a single io_uring call, and the scheduler drops fd-related `EventSource` variants (only keeps `Futex` and `IoUring`). Per-source watcher lists and io_uring machinery stay the same. Userspace helpers in `toyos-abi` would wrap the ring setup for simple blocking I/O.

## Known issues

<!-- Track blocking issues and findings here. Remove when resolved. -->
- Profiling tooling is missing — no way to measure performance inside ToyOS yet.
- FS base uses `wrmsr`/`rdmsr` on every context switch — should use FSGSBASE instructions (`wrfsbase`/`rdfsbase`) which are 5-20x faster. Requires setting CR4.FSGSBASE at boot.
- APIC is xAPIC (memory-mapped) — should switch to x2APIC (MSR-based). Eliminates MMIO mapping, gives 32-bit APIC IDs (>255 CPUs), and fixes the ICR high/low write race. QEMU needs `-cpu ...,+x2apic`.
- No PCID — every context switch and TLB shootdown does a full TLB flush. PCID would tag TLB entries per-address-space, avoiding flushes on CR3 switch. Also no per-page `invlpg` — shootdowns IPI all CPUs for a full flush.
- LAPIC timer uses one-shot mode — should use TSC deadline mode (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. TSC is already calibrated for `nanos_since_boot()`.
- SMEP not enabled — SMAP is on but SMEP (CR4 bit 20, prevents kernel executing user pages) is not. Trivial to enable.
