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

## Dependencies

Only **Rust** and **QEMU** (for development). Everything else is bootstrapped from Rust:

- **toyos-ld** (`toyos-ld/`) — Custom linker. Used for bootloader, kernel, and all userland programs.
- **toyos-cc** (`toyos-cc/`) — Minimal C compiler. Not meant to grow — exists to bootstrap C compilers (tinycc) and compile doomgeneric.
- **rust/** — Fork of the Rust compiler and std with ToyOS platform support (submodule). Auto-bootstraps. Kept up to date with upstream.

## Ecosystem forks

Nothing is vendored. Every forked crate lives in its own repository as a `toyos`
branch based on a pinned upstream commit, consumed via `[patch.crates-io]` git
dependencies. **`forks.toml` is the manifest** — upstream, base commit, delta
size, tier, and PR status for each. Keep it accurate; it is how the estate stays
honest.

- A fresh `git clone` + `cargo run` works with no setup: cargo fetches the forks.
- To *edit* a fork: clone it beside the monorepo and list it in `.cargo/config.toml`
  (gitignored — see `.cargo/config.toml.example`). Commit and push to the fork
  repo; the monorepo only pins the branch.
- `git log <base>..toyos` in a fork is exactly the ToyOS delta — and exactly what
  an upstream PR should contain.
- Forks depend on ToyOS crates by **version**, never by path: a path escaping the
  fork's own repo cannot resolve once cargo checks it out alone. `[patch]` in the
  monorepo redirects those to the working tree.

Rules:

- All changes must be upstream-mergeable. ToyOS aims to be a first-class platform in every important Rust ecosystem crate, alongside unix and windows.
- Use `#[cfg(target_os = "toyos")]` — never hijack existing cfg gates.
- Add ToyOS as a new platform alongside existing ones. Don't modify cross-platform code.
- Publishing `toyos-abi`, `toyos`, and `window` to crates.io is the one blocker
  for actual upstream PRs (upstream cannot depend on unpublished crates). It is
  *not* needed for local builds — `[patch]` resolves unpublished crates fine.

## POSIX compatibility

The kernel ABI and SDK are Rust-native and capability-shaped. POSIX
compatibility, where it is needed to run ecosystem software, belongs in a
**userspace** compatibility layer (`userland/libc` — ToyOS's own code, not a
fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

This is a settled question, decided by experiment. The `unix_compat_try` branch
(2026-03) tried the opposite: make `rust-lang/libc` ToyOS's platform layer so
crates would not need forks. It regressed the ToyOS-hosted rustc (the build's
hard assert was downgraded to a warning), dropped cargo from `system.toml`, and
pulled `aws-lc-rs` + `cmake` into the dependency graph the moment ToyOS claimed
to be a Unix-ish libc platform. It never booted. Notably it never touched
`toyos-abi` — the experiment was always userspace impedance-matching, and it
failed only because it sat *below* std instead of beside it. See
`specs/posix-bootstrap-cost.md` for what running autotools software would cost.

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
- `system.toml` defines which programs to build and the init sequence.

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

- Stay focused on the current task. Write findings and issues into CLAUDE.md, don't go fix them — a separate agent will handle it.
- After each task, audit CLAUDE.md and update if the architecture or project state changed.
- If something is blocking, stop and report it. Don't work around it.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce output. If a command produces a lot of output or takes long, run it in the background — background tasks automatically get their output written to a file.
- **`cargo test` and `cargo run` produce large output** (std rebuild warnings, initrd listing, serial output). Always run them in the background so the Bash tool doesn't silently truncate the output — `... [N characters truncated] ...` in tool output means data was lost. Read the output file afterward.
- **Always be empirical.** Never assume a command succeeded or failed — read the actual output. Never assume code works — run it. Never guess at root causes — investigate. Guessing is unproductive; verify everything.
- **ToyOS is fast.** Full boot completes in under a second. Build is incremental and usually finishes in seconds. Never assume things are slow.
- **Always poll before sleeping.** Try reading output immediately. Only add a short delay if the poll came back empty.
- **Never `git add -A` or `git add .`.** Stage explicit paths and check `git diff --cached --name-only` before committing. Several agents commit to this tree concurrently; a bare `add -A` has already swept up another agent's in-progress work. This also applies inside the forks.
- **Concurrent measurement is unreliable.** Another agent building or booting QEMU in the same tree perturbs timings and shares the cargo target lock. When measuring a flaky test's rate, A/B against the same HEAD in the same session rather than comparing to a number someone recorded earlier.

## Ideas

- **io_uring as the only blocking I/O mechanism.** Currently the kernel has two parallel notification paths: `scheduler::block(event)` for direct thread blocking and `io_uring::complete_pending_for_event` for ring watchers. Every wake site does both. If all fd-based blocking went through io_uring, blocking syscalls (read, write, accept) become non-blocking try-once-and-return, wake sites become a single io_uring call, and the scheduler drops fd-related `EventSource` variants (only keeps `Futex` and `IoUring`). Per-source watcher lists and io_uring machinery stay the same. Userspace helpers in `toyos` would wrap the ring setup for simple blocking I/O.

- **Capability-based resource model.** Replace global Pid/Tid integers with per-process handles (like Zircon's `zx_handle_t`). Every kernel object — processes, threads, pipes, shared memory, devices — gets referenced via handles that encode both identity and rights. A process can only operate on objects it holds a handle to. This unifies fds, Pids, and Tids into one mechanism, eliminates confused-deputy bugs, and enables fine-grained rights delegation. The per-process fd table is already halfway there. Zircon (Fuchsia) and seL4 are the reference designs.

## Diagnostics roadmap

Three layers, built in order. Each layer is useful on its own.

**Layer 1: Process accounting (counters).** Add cumulative counters to ProcessData — wall time, user/kernel CPU time, page fault count (by cause: demand, zero, shared), I/O op count + bytes, time blocked (by reason: I/O, futex, IPC, runqueue). Increment at existing kernel sites (fault handler, NVMe completion, scheduler block/wake, syscall entry/exit). One new syscall to read them. Userland `stats` tool: spawn child, wait, read counters, print summary. Answers "what kind of problem is this?" with near-zero overhead.

**Layer 2: Event tracing (timestamped log).** Per-process ring buffer of `(timestamp_ns, TraceEvent)` entries. Events: syscall entry/exit, fault entry/exit (with address + cause), I/O submit/complete, scheduled (with runqueue wait duration), preempted (with timeslice used), blocked/woken (with reason), lib load, lib relocate. ~24 bytes per event, 4096-entry ring (~96KB). Instrument ~8 kernel sites. One new syscall to read the ring. Userland `trace` tool: spawn child, wait, read events, print timeline with durations. Answers "where exactly is time going, in what order?"

**Layer 3: RIP sampling (statistical profiler).** Only useful once Layer 1/2 confirm something is CPU-bound. Per-process ring buffer of `(timestamp_ns, rip)` samples recorded on timer tick. Needs frame-pointer-based stack unwinding to be useful (flat RIP profiles without call stacks are nearly worthless). Build only when CPU-bound userspace code becomes an actual problem.

## Planned architecture (specs written, implementation staged)

Three judge-reviewed specifications drive the next phases; read them before touching
the subsystems they cover:

- `specs/scheduler-core-spec.md` — ownership-typed scheduler core as a `no_std` crate
  with a host-side deterministic simulator + interleaving fuzzer; per-CPU exclusive
  queues with message-passing wakes; 10-stage always-green migration.
  **Migration state: Stage 4 done.** `toyos-sched/` holds the core crate (Stage 0
  boundary types + `fair.rs`, the relocated vruntime/lag/frontier policy the kernel
  now calls, the Stage 3 primitives — `mailbox.rs` is the crate's only `unsafe`,
  everything else is `deny(unsafe_code)`; `waitq.rs`, `retire.rs`, `task.rs` state
  word — and the Stage 4 machine: the linear `Task` value with its five lifecycle
  types (`task.rs`), `queue.rs`, `timer.rs`, `msg.rs`, `invariants.rs` and the
  per-CPU `cpu.rs` (`CpuSched`, the `SchedPass` type-state, `Action`, the sleep
  handshake). `toyos-sched/sim/` is the deterministic simulator (VM, explorer,
  ChoiceStream with seed/fuzz-byte/PCT/replay drivers, shrinker, corpus, scenario
  library); `toyos-sched/loom/` the model-checking harness. **The kernel still does
  not call any of it** beyond `fair.rs` — `Hw` is Stage 6, cutover is Stage 7.
  **Stage 5 done** (one commit per source): every wait site named in the spec —
  pipes, futex, listener, audio fd, io_uring, join — now registers before it
  re-checks and parks with the registration in hand. `kernel/src/waitq.rs` is the
  shim: `WaitQueue`/`WaitTicket` in the shape of `toyos_sched::waitq`, registrations
  held in the old blocked pool so the lock every wake path already takes arbitrates
  them; `scheduler::block_on` takes the ticket by value, so a park cannot reach the
  pool without one. The per-source rechecks are one recheck, and `FUTEX_WAKE_GEN`,
  `FUTEX_LOCK` and `SwitchReason::BlockFutex` are deleted. Not converted (not in the
  spec's list, and they keep `pending_wakes` and the legacy `source_ready` recheck
  alive until Stage 8): keyboard, serial-console and network reads. Honest scope —
  the practical decide-to-park window is closed; the structural, message-serialized
  closure is Stage 7, which deletes the shim.
  Host tests: `cargo test` inside `toyos-sched/` (unit + loom + sim; ~15 s). Two
  proofs that the harnesses have teeth, both required:
  `TOYOS_LOOM_RAW=1 cargo test -p toyos-sched-loom --features no-preempt-guard`
  must FAIL (it does, on invariant I2), and the simulator's `old_steal_port`
  scenario — a port of the OLD steal-and-scan algorithm — must fail while the same
  workload passes under the new protocol (it does, on I1/I6 and on I8, the
  address-space-freed-under-a-live-task detector). The full Stage 4 exit criterion
  runs from the CLI, not from `cargo test`:
  `cargo run --release -p toyos-sched-sim -- gate 10000` (10⁴ seeds/scenario) and
  `-- fuzz-sweep 10000000` (10⁷ fuzz steps/scenario).
  Gate A (audio glitch) runs inside `cargo test`: each audio test boots at smp=1
  and smp=8 and is gated on the recorded histograms in `tests/audio-baseline.toml`
  (all-clean = strict). **That baseline is optimistic and Gate A is not currently
  trustworthy** — see the known issue on `audio_tone_load`. Stage 2 landed: per-CPU `kernel/src/irq_ring.rs`
  carries `(IrqSource, ts)` IRQ-time stamps for audio/net/xHCI; ISRs publish + set
  need_resched via the shared `msix_entry!` macro; `drain_events` consumes the ring
  (no global pending flags, no cpu==0 gating).
- `specs/capability-handles-spec.md` — refcounted kernel objects behind typed
  per-process handles (Fd→Handle); subsumes the SharedToken/io_uring/Fd items below.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one
  wait-free completion primitive, one park/recheck site.

## Known issues

<!-- Track blocking issues and findings here. Remove when resolved. -->
- **`audio_tone_load` at smp=1 is flaky on an unmodified tree, so the recorded audio baseline is wrong.** Measured 2026-07-28 during Stage 5: 2 failures in 19 baseline runs (1- and 8-period gaps), with soundd's own underrun count spanning 0–17 across runs that all "passed". `tests/audio-baseline.toml` records all-clean, so Gate A both under-reports real glitches and will fire false failures during the scheduler migration — the one gate the migration most depends on. Two things to settle, in order: (1) is the residual a real single-core underrun or harness noise (soundd reporting non-zero underruns on passing runs says real), and (2) record an honest histogram rather than an aspirational one. Until then, treat a single green audio run as evidence of nothing. Note this does not undo the audio work — the pre-fix baseline was 1,834 gaps — but "0 gaps, verified" was too strong a claim.
- Profiling tooling is incomplete — Layer 1 (process accounting counters + `stats` tool) is implemented. Layer 2 (event tracing) and Layer 3 (RIP sampling) are not yet built. See Diagnostics roadmap.
- **~half the CPU is unattributed kernel time on single-core under load.** With the Doom demo on `--smp 1`, per-process CPU sums to ~45% while the core runs ~97% busy. Prime suspect: context-switch volume (audio cycles + game + compositor ≈ 1000+ switches/s) with address-space switches, expensive under TCG. Quantify (needs Layer 2 tracing or a switch counter) before optimizing; this is the main obstacle to full-speed single-core Doom.
- soundd idles at ~7% of a core (mixing/dithering silence at period rate even with no clients). Acceptable but worth revisiting with the idle policy in spec §5.8.
- `ps` state column (`task_sched_state`) still uses try_lock and skips `outgoing` — can transiently misreport under load (display-only; CPU-time column is accurate).
- One unreproduced observation: `ps` appeared to stall for >2s under heavy single-core load (later runs fine). If seen again, capture with LLDB before restarting.
- A panic *while holding* `PROCESS_TABLE` hangs the panicking CPU: `try_recover_from_panic` lands in `cpu_idle_loop`, which takes that lock unconditionally every iteration, and the dead thread never releases it. Pre-existing and unchanged by the panic-recovery fix (a `try_lock` could not have saved it either — a spinlock's `try_lock` fails for its own holder too). The general shape — locks a dead thread can strand — belongs to the capability-handles/ownership work.
- `SYS_AUDIO_SUBMIT` has no ownership check — any process can submit audio buffers without holding the audio fd. Gate on the claimed device (or fold into capability handles).
- The build system suppresses rustc warnings on success (`src/build.rs` uses `Command::output()`, which captures stderr whether or not quiet mode is on), so the zero-warning bar is not enforced by `cargo run -- --build-only`. The kernel itself builds with `-D warnings`, so its warnings are errors anyway; everything else is unchecked. To see a crate's warnings meanwhile: `cd kernel && touch src/main.rs && env -u RUSTFLAGS -u RUSTC RUSTUP_TOOLCHAIN=toyos PATH=<repo>/toyos-ld/target/<host>/release:$PATH cargo build --target x86_64-unknown-none`.
- Panic-path residual: a panic while the virtio-console TX queue is wedged *and* unlocked still spins in `submit_and_wait`; bounding that wait is a virtio.rs semantics change needing its own discussion.
- Pre-existing deadlock hazard: `timer_handler` (IRQ context, IF=0) → `xhci::poll_if_pending` → `XHCI.lock()` (spinning ticket lock) can deadlock if the timer interrupts the same CPU's thread while it holds the XHCI lock via the fd.rs keyboard/mouse read poll. A `try_lock` in the timer path would remove it.
- `tests/toyos-rust-tests/system.toml` is dead (only `tests/testcases` is used) — delete.
- PCID + INVPCID codepaths untested on real hardware (QEMU TCG doesn't support either). Both are CPUID-gated — TCG falls back to CR3 reload. Needs testing on KVM or bare metal.
- TLB shootdowns still IPI all CPUs for a full flush. Per-page targeted shootdowns not yet implemented.
- LAPIC timer uses one-shot mode — should use TSC deadline mode (`IA32_TSC_DEADLINE` MSR) for precise absolute-time wakeups. TSC is already calibrated for `nanos_since_boot()`.
- **io_uring abuses shared_memory.** io_uring doesn't share memory between processes — it shares a page between kernel and one userspace process. It should own its `PageAlloc` directly, map it into the process's page tables, and store it in `IoUringInstance`. Drop frees the pages. No shared_memory involvement. This also removes the only caller of `shared_memory::destroy()`.
- **`SharedToken` is a bare `u32` — no RAII.** Unlike `PhysPage` (which can't leak because Drop returns it to the PMM), `SharedToken` is `Copy` with no destructor. The caller must remember to call the right cleanup function. It should be a non-Copy RAII handle: Drop removes the region and frees backing pages. Expose `.raw()` for the numeric value to pass to userspace, but keep the owning handle in kernel data structures.
- **No physical memory fairness.** Any process can allocate unbounded physical memory until the system runs out. There are no per-process limits, no memory pressure signals, and no OOM killer. A single misbehaving process can starve the entire system.
- **`build_toyos_bins` belongs in the test harness, not `src/build.rs`.** It's only called from the test harness and contains test-specific logic (cdylib subcrate discovery, `-L` rustflags for `.so` linking). Move it to `tests/`.
- **`Fd` is a Unix-ism.** ToyOS has no files-are-everything model. The integer identifies pipes, devices, io_uring instances, IPC connections — it's a handle, not a file descriptor. Rename `Fd` → `Handle` to match what it actually is. Aligns with the capability-based direction.
- **Keyboard poll readiness is spurious on repeated HID reports.** `HidDevice::dispatch_report` wakes `EventSource::Keyboard` watchers on every report, but `keyboard::handle_report` diffs against the previous report and queues zero events for identical ones (host key auto-repeat produces a stream of these). A blocking `read` after such a wake parks the reader until the next real key event — this froze the compositor (and the whole display) while a key was held. Userland now reads the keyboard/mouse fds non-blocking, but the kernel should only wake watchers when `handle_report` actually queued events. Also inconsistent: `sys_read` on an empty Keyboard fd blocks, on an empty Mouse fd returns `NotFound`.
- **`bootstrap-cc` is unreachable and unpinned.** Nothing references it — no build code, no `system.toml` entry, no test; it is excluded from the userland workspace. It also inherits `userland/rust-toolchain.toml`, so a bare `cargo check` in its directory cross-compiles a host-only tool to the ToyOS target and fails in `ring`/`getrandom`; it must be built with `--target <host>`. Its TinyCC download is https now but still unverified — repo.or.cz serves an on-demand cgit snapshot whose gzip wrapper is probably not byte-reproducible, so pinning a checksum needs a stable release tarball first. Decide whether this tool is alive; if it is, wire it into the build and pin the download.
- **The `memmap2` fork is 165 lines of unreachable code.** `rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates `target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and userland lists memmap2 under `[patch.unused]` — so no ToyOS code path calls any memmap2 API. `src/toyos.rs` is compiled and never called; the fork's only load-bearing content is the `0.9.10 → 0.2.1` version relabel that satisfies rustc's pin. Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in `rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and `map_anon`, are correct in the fork). Exactly one of the two should exist. Three real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.
