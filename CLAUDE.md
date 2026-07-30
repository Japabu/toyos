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
- **No slop comments.** Never add comments that restate what the code does. No "auto-closes on drop", "returns the value", "loop through items". Comments explain *why*, not *what*. If the code needs a *what* comment to be understood, rewrite the code.

## Architecture

> The architecture is under active development. Details here are a snapshot — always read the code for current state.

**Kernel** — Minimal. New additions to the kernel must be discussed and justified. Currently handles: resource management, scheduling, process lifecycle, filesystem, device arbitration.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel, maps hardware buffers into its own memory, and serves clients over IPC. Crash a daemon, the kernel is fine.

**IPC** — Named services via listen/accept/connect. Pipes backed by shared-memory ring buffers.

**Memory** — 2MB pages only. Demand paging. Shared memory across processes.

**Processes** — PIE binaries. Spawn-based. Full SMP. dlopen/dlsym for shared libraries.

**Scheduling** — Efficient, event-driven, fair-share. Per-CPU run queues. Must scale to 128+ cores without excessive overhead.

**Filesystem** — VFS with mount points. Initrd, tmpfs, NVMe.

**Syscall ABI** — Defined in `toyos-abi/`. The ABI is the contract between kernel and userland. Includes struct layouts, syscall numbers, constants, and typed syscall wrappers. Completely unstable — read the code for current state. Never add or change a syscall without discussion. The `toyos/` crate builds on top with typed handles, IPC framing, and service helpers — userland code uses `toyos`, kernel uses `toyos-abi` only.

**Kernel must never crash from userland.** A buggy userland process must not be able to bring down the kernel. But if the kernel itself has a bug, it must crash loudly so we can fix it and harden.

Corollary: **fail-fast is for kernel bugs, not for untrusted input.** An `expect()` on a value that crossed the trust boundary is a userland-triggered kernel panic wearing fail-fast's clothes. Untrusted input that cannot be satisfied returns `SyscallError::{ResourceExhausted, InvalidArgument}` — never a panic, and never a silent truncation of the request to make it fit. Two bounds enforce this today: `user_ptr::MAX_USER_STR` (64 KiB, on the primitive, sized so the *derived* allocations stay under the 2 MiB ceiling) and `fd.rs`'s `MAX_FDS` (1024, inside `FdTable`'s only two insert primitives). Both numbers are policy, not physics.

## Dependencies

Only **Rust** and **QEMU** (for development). Everything else is bootstrapped from Rust:

- **toyos-ld** (`toyos-ld/`) — Custom linker. Used for bootloader, kernel, and all userland programs.
- **toyos-cc** (`toyos-cc/`) — Minimal C compiler. Not meant to grow — exists to bootstrap C compilers (tinycc) and compile doomgeneric.
- **rust/** — Fork of the Rust compiler and std with ToyOS platform support (submodule). Auto-bootstraps. Kept up to date with upstream.

## Ecosystem forks

Nothing is vendored. Every forked crate lives in its own repository as a `toyos` branch based on a pinned upstream commit, consumed via `[patch.crates-io]` git dependencies. **`forks.toml` is the manifest** — upstream, base commit, delta size, tier, and PR status for each. Keep it accurate; it is how the estate stays honest.

- A fresh `git clone` + `cargo run` works with no setup: cargo fetches the forks.
- To *edit* a fork: clone it beside the monorepo and list it in `.cargo/config.toml` (gitignored — see `.cargo/config.toml.example`). Commit and push to the fork repo; the monorepo only pins the branch.
- `git log <base>..toyos` in a fork is exactly the ToyOS delta — and exactly what an upstream PR should contain.
- Forks depend on ToyOS crates by **version**, never by path: a path escaping the fork's own repo cannot resolve once cargo checks it out alone. `[patch]` in the monorepo redirects those to the working tree.

Rules:

- All changes must be upstream-mergeable. ToyOS aims to be a first-class platform in every important Rust ecosystem crate, alongside unix and windows.
- Use `#[cfg(target_os = "toyos")]` — never hijack existing cfg gates.
- Add ToyOS as a new platform alongside existing ones. Don't modify cross-platform code.
- Publishing `toyos-abi`, `toyos`, and `window` to crates.io is the one blocker for actual upstream PRs (upstream cannot depend on unpublished crates). It is *not* needed for local builds — `[patch]` resolves unpublished crates fine.

## POSIX compatibility

The kernel ABI and SDK are Rust-native and capability-shaped. POSIX compatibility, where it is needed to run ecosystem software, belongs in a **userspace** compatibility layer (`userland/libc` — ToyOS's own code, not a fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

Settled by experiment. The `unix_compat_try` branch (2026-03) tried the opposite — make `rust-lang/libc` ToyOS's platform layer so crates would not need forks. It regressed the ToyOS-hosted rustc, dropped cargo from `system.toml`, pulled `aws-lc-rs` + `cmake` into the dependency graph the moment ToyOS claimed to be a Unix-ish libc platform, and never booted. It never touched `toyos-abi`: the experiment was always userspace impedance-matching, and it failed only because it sat *below* std instead of beside it. `specs/posix-bootstrap-cost.md` has what running autotools software would cost.

## Std library rules

- **Add ToyOS as a new platform alongside unix/windows/wasi — never hijack existing cfg gates.** This is the rule that actually governs; the two below are its consequences.
- Prefer ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`, anything with `toyos` in the path.
- A cross-platform file may be touched **only to add a target arm to an existing platform-dispatch site** — never to change cross-platform semantics or API shape. 54 files in the fork have no `toyos` in their path (`sys/*/mod.rs` dispatch, `rustc_target/src/spec/`, `os/fd/`); every one is a dispatch arm. `library/alloc` and `library/core` have **zero** delta and should keep it.
- Corollary: cherry-picking an already-**merged** upstream commit is early convergence, not divergence, and is allowed. Copying an unmerged PR is not — it voids the promise that the fork delta is what an upstream PR would contain.

## Build & test

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU.
- `cargo run -- --build-only` builds everything without launching QEMU.
- `cargo test` runs integration tests (boots QEMU headless, runs test harness inside ToyOS).
- `cargo test -- --nocapture` same but with serial output visible.
- `cargo test -- process_stats` runs only tests matching "process_stats" (substring filter).
- `cargo test -- process_stats --nocapture` filter + serial output.
- `cargo test -- --list` lists all test names without running them.
- `cargo test --test toyos-build -- --audio-gate 30` runs gate A's thorough tier (~17 min).
- `system.toml` defines which programs to build and the init sequence.

**Gate A (audio) has two tiers.** `tests/audio-baseline.toml` documents both in full and justifies every number in it.

- **Fast** — part of every `cargo test`. One boot per config. Certifies the per-run counter ceilings, that the instrument is alive, and that audio does not *reproducibly* drop out (a dropout re-boots once; only a second one fails). It certifies nothing about a rate — one run is one sample.
- **Thorough** — `cargo test --test toyos-build -- --audio-gate N`. N iterations of all four configs; every per-run outcome becomes a rate or a distribution, compared against the recorded sample by Mann-Whitney (counters) and Fisher exact (yes/no outcomes). **A scheduler-migration stage transition gates on this tier.** At N=30 it detects a 25% shift in wake lateness and a 5% drop in soundd's wake count 99.9% of the time, with a 0.25% false-red rate on a clean tree; it does *not* detect a doubling of the dropout rate, and no N a human waits for would.

The gate's four instrument defects and the dropout regression they hid are closed and written up in `specs/audio-gate-history.md`. The reusable lesson: these counters drift between batches on one host with no code change, so only same-session A/B numbers mean anything.

## Repository layout

```
src/              Build system (the root cargo project, package name: toyos-build)
kernel/           Kernel
bootloader/       UEFI bootloader
userland/         All userland programs + ecosystem forks
toyos-abi/        Kernel ABI (types, constants, syscall numbers, syscall wrappers)
toyos/            Userland SDK (typed handles, IPC, services, shm, net, Ring)
toyos-ld/         Custom linker
toyos-cc/         Custom C compiler
rust/             Rust compiler/std fork (submodule)
tests/            Integration tests (QEMU-based)
specs/            Specifications, investigation write-ups, known issues
system.toml       What to build and boot
```

## Debugging

**LLDB via QEMU** — All binaries are PIE, addresses change every boot. Parse serial output for `Kernel memory located at: 0x...` to load symbols with `--slide`. For userland, serial logs pid and base address at `spawn:`. Use `breakpoint set -r <pattern>` for Rust symbols (not `-n`, which doesn't work with `::` paths).

**Via full OS**: `cargo run` in background, attach LLDB to `gdb-remote 1234`. `--debug` flag pauses kernel before init via `DEBUG_WAIT` AtomicBool (release it with `memory write -s 1 <DEBUG_WAIT addr + slide> 0`), enables QEMU's `-d int,cpu_reset` exception log at `/tmp/toyos-qemu-debug.log`, and parks QEMU on triple fault (`-action shutdown=pause`) so the faulting CPU state stays inspectable via gdb/QMP.

**Audio verification**: `cargo run -- --smp N --dump-audio` captures device output to `/tmp/toyos-audio.wav` (parse to EOF — RIFF sizes stay 0 unless the guest shuts down cleanly). `cargo test -- audio` runs the single-core glitch regression tests (zero mid-signal gaps asserted). Doom's demo loop starts ~5s after its menu goes idle; soundd prints wake/underrun/latency stats every ~2s while clients exist; doom prints `[music]` synthesis real-time-factor telemetry every ~5s while music plays.

**QMP** (QEMU Machine Protocol) — Socket at `/tmp/toyos-qmp.sock`. Script at `.claude/qmp.py`:
- `python3 .claude/qmp.py "ls /bin"` — type string + Enter
- `python3 .claude/qmp.py --raw ret` — single key
- `python3 .claude/qmp.py --raw n --ctrl` — Ctrl+N
- `python3 .claude/qmp.py --screenshot /tmp/toyos-screen.png` — capture screen

## Workflow

- Stay focused on the current task. Record findings and issues in `specs/known-issues.md`, don't go fix them — a separate agent will handle it. Add a one-line summary here only if a future agent must not miss it.
- After each task, audit CLAUDE.md and update if the architecture or project state changed. **Keep this file under ~200 lines**: it loads into every session and every subagent. Detail belongs in `specs/`, resolved narrative in `git log`.
- If something is blocking, stop and report it. Don't work around it.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce output. If a command produces a lot of output or takes long, run it in the background — background tasks automatically get their output written to a file.
- **`cargo test` and `cargo run` produce large output** (std rebuild warnings, initrd listing, serial output). Always run them in the background so the Bash tool doesn't silently truncate the output — `... [N characters truncated] ...` in tool output means data was lost. Read the output file afterward.
- **Always be empirical.** Never assume a command succeeded or failed — read the actual output. Never assume code works — run it. Never guess at root causes — investigate. Guessing is unproductive; verify everything.
- **ToyOS is fast.** Full boot completes in under a second. Build is incremental and usually finishes in seconds. Never assume things are slow.
- **Always poll before sleeping.** Try reading output immediately. Only add a short delay if the poll came back empty.
- **Never `git commit --amend`, `git rebase`, or any history rewrite.** Other agents commit to this tree concurrently; an amend has already silently rewritten another agent's commit (recovered from the reflog, but only because it was noticed immediately). Always add a new commit.
- **Wait in the FOREGROUND; do not tight-poll a background job.** Background-task notifications work reliably for the *main* agent, but spawned subagents do not stop cleanly when they background work and wait — observed repeatedly, cause unknown. That is why polling looks necessary to them. It is not. Run long commands in the foreground with an explicit `timeout` (default is only 120000 ms; the max is 600000). For work exceeding 10 minutes, background it once and block with two or three long foreground waits on a marker file (`until [ -f done ]; do sleep 20; done`, or a FIFO `cat`) — never hundreds of short polls. Note this is the opposite of what the main agent's tooling advises; subagents differ.
- **Never `git add -A` or `git add .`.** Stage explicit paths and check `git diff --cached --name-only` before committing. Several agents commit to this tree concurrently; a bare `add -A` has already swept up another agent's in-progress work. This also applies inside the forks.
- **Concurrent measurement is unreliable.** Another agent building or booting QEMU in the same tree perturbs timings and shares the cargo target lock. When measuring a flaky test's rate, A/B against the same HEAD in the same session rather than comparing to a number someone recorded earlier.

## Ideas

- **io_uring as the only blocking I/O mechanism.** The kernel has two parallel notification paths: `scheduler::block(event)` for direct thread blocking and `io_uring::complete_pending_for_event` for ring watchers, and every wake site does both. If all fd-based blocking went through io_uring, blocking syscalls become non-blocking try-once-and-return, wake sites become a single io_uring call, and the scheduler drops fd-related `EventSource` variants. Userspace helpers in `toyos` would wrap the ring setup.
- **Capability-based resource model.** Replace global Pid/Tid integers with per-process handles (like Zircon's `zx_handle_t`) that encode both identity and rights. Unifies fds, Pids and Tids into one mechanism, eliminates confused-deputy bugs, enables fine-grained delegation. The per-process fd table is already halfway there. Zircon and seL4 are the reference designs.

## Diagnostics roadmap

Three layers, built in order; each is useful on its own.

1. **Process accounting (counters).** Cumulative per-process wall and user/kernel CPU time, page faults by cause, I/O ops and bytes, time blocked by reason. Incremented at existing kernel sites, read by one syscall, printed by a userland `stats` tool. **Built.**
2. **Event tracing.** Per-process ring of `(timestamp_ns, TraceEvent)`: syscall and fault entry/exit, I/O submit/complete, scheduled (with runqueue wait), preempted (with timeslice used), blocked/woken (with reason), lib load/relocate. ~24 bytes/event, 4096 entries (~96 KB), ~8 instrumented sites, one syscall to read. Answers "where is time going, in what order?"
3. **RIP sampling.** Per-process `(timestamp_ns, rip)` ring on timer tick. Needs frame-pointer unwinding to be worth anything — flat RIP profiles without call stacks are nearly worthless. Build only once layers 1–2 confirm something is CPU-bound.

## Planned architecture (specs written, implementation staged)

Read the spec before touching the subsystem it covers.

- `specs/scheduler-core-spec.md` — ownership-typed scheduler core as a `no_std` crate (`toyos-sched/`) with a deterministic host simulator and interleaving fuzzer; per-CPU exclusive queues, message-passing wakes, 10-stage always-green migration. **Stage 7b done: the kernel drives the core, with balance on.** The driver half is `kernel/src/sched/`; `kernel/src/scheduler.rs` is the kernel-facing API and nothing else. Host tests: `cargo test` inside `toyos-sched/` (~15 s). Stage 4's exit criterion runs from the CLI: `cargo run --release -p toyos-sched-sim -- gate 10000` and `-- fuzz-sweep 10000000`. Five negative gates prove the harnesses have teeth — do not weaken one to make a change pass. Migration state, the gates, and every defect the cutover found: `specs/scheduler-migration-log.md`.
- `specs/capability-handles-spec.md` — refcounted kernel objects behind typed per-process handles (Fd→Handle); subsumes the SharedToken/io_uring/Fd debt in known issues.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one wait-free completion primitive, one park/recheck site.

## Known issues

One line each. **`specs/known-issues.md` has the detail and is the file to update.** Nothing stays here after it is fixed — the code and `git log` carry that.

- **Process isolation does not hold.** `SYS_PIPE_OPEN`/`SYS_SOCKET_CREATE`/`SYS_PIPE_MAP` check nothing and PipeIds are small sequential integers, so any process can attach to any other's pipe or socket and read, inject, or map its raw 2 MiB ring page. Also ungated: `SYS_LISTEN` (no namespace, so a service name can be impersonated), `SYS_GRANT_SHARED` (re-grantable, unvalidated target, no revoke), `SYS_SET_KEYBOARD_LAYOUT`. Decide per syscall whether it wants a one-line `device::is_owner` gate or should fall out of capability handles.
- **A crafted ELF still panics the kernel.** `vaddr_to_file_offset` (`loader.rs:265`) has no failure path. Separately, `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op, so VA exhaustion reaches `.expect` at `syscall.rs:1435`.
- **No physical memory fairness.** No per-process limits, no pressure signal, no OOM killer; one process can starve the system.
- **A panic that needs the allocator lock its own thread holds produces no output at all.** One-line fix known (`panic_flush()` at `main.rs:135`). Its own task — do not fold it into an unrelated commit.
- **A panic while holding `PROCESS_TABLE` hangs that CPU**, and a panic while the virtio-console TX queue is wedged spins in `submit_and_wait`.
- **The virtio-console has no line atomicity between writers.** Kernel `log!` and userspace `println!` interleave mid-word, corrupting machine-readable serial output; `tests/common/audio.rs` works around it reader-side. Whole-line writes should be atomic.
- **`timer_handler` → `xhci::poll_if_pending` → `XHCI.lock()` can deadlock** against the same CPU's `fd.rs` keyboard/mouse poll. A `try_lock` in the timer path removes it.
- **Keyboard poll readiness is spurious on repeated HID reports** — the kernel wakes watchers for reports that queue no events, which froze the compositor while a key was held. Userland reads non-blocking to compensate.
- **A thread retired while parked leaks its wait-queue node.** A leak, not a correctness hole; the fix belongs with the intrusive `wait_node` the core still owes.
- **`handle_retire`'s `need_resched` on a running target is a request the next pass may decline.** Conformant (§7.6 bounds it by the quantum), one atomic load from meaning what it says.
- **soundd has six audit defects open**: hardcoded cpal format, per-client shm never freed, `virtio_sound` ignores the PCM caps it queries, correlated TPDF dither draws, non-unity passthrough gain, uninitialised padding in `AudioInfo::as_bytes`.
- **soundd idles at ~7% of a core** mixing silence at pipeline rate. Harmless; revisit with the idle policy in audio spec §5.8.
- **Gate A can still fail a run on `drains` alone**, with no gap and no underrun. A per-run failure should require evidence of harm.
- **`ps`/`stats`/`dump_blocked` lost their cross-CPU view at Stage 7a.** The first two were rebuilt on published counters and are accurate; `dump_blocked` now prints only the calling CPU's parked map.
- **CORRECTED: the "~half the CPU is unattributed kernel time" claim was wrong, and its sign was backwards.** One accumulator feeds both numerators, so unattributed time cannot open a gap between them — true busy *exceeds* 97%. The 45-vs-97 gap is reader-side: `ps` compares a lifetime average against the taskbar's one-second delta. `specs/cpu-attribution.md`.
- **Profiling layers 2 and 3 are not built** (event tracing, RIP sampling). Layer 1 is.
- **The build system suppresses rustc warnings on success** — `Command::output()` in `src/build.rs` captures stderr regardless of quiet mode, so the zero-warning bar is unenforced outside the kernel.
- **`bootstrap-cc` is alive but not wired in** — nothing builds it, it inherits the wrong toolchain, and its TinyCC download is unpinned.
- **The `memmap2` fork is 165 lines of unreachable code.** Delete `src/toyos.rs` or drop the toyos gate in `rustc_data_structures` — exactly one of the two.
- **Design debt:** io_uring abuses `shared_memory`; `SharedToken` is a bare `u32` with no RAII; `Fd` should be `Handle`; `build_toyos_bins` belongs in the test harness; `Lock::force_unlock` has no caller.
- **Hardware gaps:** PCID/INVPCID untested outside TCG; TLB shootdowns IPI every CPU for a full flush; the LAPIC timer is one-shot where TSC-deadline would be exact.
- One unreproduced observation: `ps` appeared to stall for >2 s under heavy single-core load. If seen again, capture with LLDB before restarting.
