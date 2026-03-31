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
- `cargo run -- --build-only` builds everything without launching QEMU.
- `cargo test` runs integration tests (boots QEMU headless, runs test harness inside ToyOS).
- `cargo test -- --nocapture` same but with serial output visible.
- `cargo test -- process_stats` runs only tests matching "process_stats" (substring filter).
- `cargo test -- process_stats --nocapture` filter + serial output.
- `cargo test -- --list` lists all test names without running them.
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
- **`cargo test` and `cargo run` produce large output** (std rebuild warnings, initrd listing, serial output). Always run them in the background so the Bash tool doesn't silently truncate the output — `... [N characters truncated] ...` in tool output means data was lost. Read the output file afterward.
- **Always be empirical.** Never assume a command succeeded or failed — read the actual output. Never assume code works — run it. Never guess at root causes — investigate. Guessing is unproductive; verify everything.

## Ideas

- **io_uring as the only blocking I/O mechanism.** Currently the kernel has two parallel notification paths: `scheduler::block(event)` for direct thread blocking and `io_uring::complete_pending_for_event` for ring watchers. Every wake site does both. If all fd-based blocking went through io_uring, blocking syscalls (read, write, accept) become non-blocking try-once-and-return, wake sites become a single io_uring call, and the scheduler drops fd-related `EventSource` variants (only keeps `Futex` and `IoUring`). Per-source watcher lists and io_uring machinery stay the same. Userspace helpers in `toyos-abi` would wrap the ring setup for simple blocking I/O.

## Diagnostics roadmap

Three layers, built in order. Each layer is useful on its own.

**Layer 1: Process accounting (counters).** Add cumulative counters to ProcessData — wall time, user/kernel CPU time, page fault count (by cause: demand, zero, shared), I/O op count + bytes, time blocked (by reason: I/O, futex, IPC, runqueue). Increment at existing kernel sites (fault handler, NVMe completion, scheduler block/wake, syscall entry/exit). One new syscall to read them. Userland `stats` tool: spawn child, wait, read counters, print summary. Answers "what kind of problem is this?" with near-zero overhead.

**Layer 2: Event tracing (timestamped log).** Per-process ring buffer of `(timestamp_ns, TraceEvent)` entries. Events: syscall entry/exit, fault entry/exit (with address + cause), I/O submit/complete, scheduled (with runqueue wait duration), preempted (with timeslice used), blocked/woken (with reason), lib load, lib relocate. ~24 bytes per event, 4096-entry ring (~96KB). Instrument ~8 kernel sites. One new syscall to read the ring. Userland `trace` tool: spawn child, wait, read events, print timeline with durations. Answers "where exactly is time going, in what order?"

**Layer 3: RIP sampling (statistical profiler).** Only useful once Layer 1/2 confirm something is CPU-bound. Per-process ring buffer of `(timestamp_ns, rip)` samples recorded on timer tick. Needs frame-pointer-based stack unwinding to be useful (flat RIP profiles without call stacks are nearly worthless). Build only when CPU-bound userspace code becomes an actual problem.

## Known issues

<!-- Track blocking issues and findings here. Remove when resolved. -->
- Profiling tooling is incomplete — Layer 1 (process accounting counters + `stats` tool) is implemented. Layer 2 (event tracing) and Layer 3 (RIP sampling) are not yet built. See Diagnostics roadmap.
- PCID + INVPCID codepaths untested on real hardware (QEMU TCG doesn't support either). Both are CPUID-gated — TCG falls back to CR3 reload. Needs testing on KVM or bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted shootdowns not yet implemented.
- LAPIC timer uses one-shot mode — should use TSC deadline mode (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. TSC is already calibrated for `nanos_since_boot()`.
- **io_uring abuses shared_memory.** io_uring doesn't share memory between processes — it shares a page between kernel and one userspace process. It should own its `PageAlloc` directly, map it into the process's page tables, and store it in `IoUringInstance`. Drop frees the pages. No shared_memory involvement. This also removes the only caller of `shared_memory::destroy()`.
- **`SharedToken` is a bare `u32` — no RAII.** Unlike `PhysPage` (which can't leak because Drop returns it to the PMM), `SharedToken` is `Copy` with no destructor. The caller must remember to call the right cleanup function. It should be a non-Copy RAII handle: Drop removes the region and frees backing pages. Expose `.raw()` for the numeric value to pass to userspace, but keep the owning handle in kernel data structures.
- **No physical memory fairness.** Any process can allocate unbounded physical memory until the system runs out. There are no per-process limits, no memory pressure signals, and no OOM killer. A single misbehaving process can starve the entire system.
