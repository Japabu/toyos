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

Corollary: **fail-fast is for kernel bugs, not for untrusted input.** An `expect()`
on a value that crossed the trust boundary is a userland-triggered kernel panic
wearing fail-fast's clothes. Untrusted input that cannot be satisfied returns
`SyscallError::{ResourceExhausted, InvalidArgument}` — never a panic, and never a
silent truncation of the request to make it fit. Two bounds enforce this today:
`user_ptr::MAX_USER_STR` (64 KiB, on the primitive, sized so the *derived*
allocations stay under the 2 MiB ceiling) and `fd.rs`'s `MAX_FDS` (1024, inside
`FdTable`'s only two insert primitives). Both numbers are policy, not physics.

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
- `cargo test --test toyos-build -- --audio-gate 30` runs gate A's thorough tier (~17 min, see below).
- `system.toml` defines which programs to build and the init sequence.

**Gate A (audio) has two tiers.** `tests/audio-baseline.toml` documents both in
full; the short version:

- **Fast** — part of every `cargo test`. One boot per config. Certifies the
  per-run counter ceilings, that the instrument is alive, and that audio does
  not *reproducibly* drop out (a dropout re-boots once; only a second one
  fails). It certifies nothing about a rate — one run is one sample.
- **Thorough** — `cargo test --test toyos-build -- --audio-gate N`. N iterations of all four
  configs; every per-run outcome becomes a rate or a distribution, compared
  against the recorded sample by Mann-Whitney (counters) and Fisher exact
  (yes/no outcomes). This is what a scheduler-migration stage transition gates
  on. At N=30 it detects a 25% shift in wake lateness 99.9% of the time and a
  5% drop in soundd's wake count 99.9%, with a 0.25% false-red rate on a clean
  tree; it does *not* detect a doubling of the dropout rate, and no N a human
  waits for would.

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
- **Never `git commit --amend`, `git rebase`, or any history rewrite.** Other agents commit to this tree concurrently; an amend has already silently rewritten another agent's commit (recovered from the reflog, but only because it was noticed immediately). Always add a new commit.
- **Wait in the FOREGROUND; do not tight-poll a background job.** Background-task notifications work reliably for the *main* agent, but spawned subagents do not stop cleanly when they background work and wait — observed repeatedly, cause unknown. That is why polling looks necessary to them. It is not. Run long commands in the foreground with an explicit `timeout` (default is only 120000 ms; the max is 600000). For work exceeding 10 minutes, background it once and block with two or three long foreground waits on a marker file (`until [ -f done ]; do sleep 20; done`, or a FIFO `cat`) — never hundreds of short polls. Note this is the opposite of what the main agent's tooling advises; subagents differ.
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
  **Migration state: Stage 7b done — the kernel drives the core, with balance on.**
  `toyos-sched/` holds the core crate (Stage 0 boundary types + `fair.rs`; the
  Stage 3 primitives — `mailbox.rs` is the crate's only `unsafe`, everything else
  is `deny(unsafe_code)`; `waitq.rs`, `retire.rs`, `task.rs` state word — and the
  Stage 4 machine: the linear `Task` value with its five lifecycle types
  (`task.rs`), `queue.rs`, `timer.rs`, `msg.rs`, `invariants.rs` and the per-CPU
  `cpu.rs`). `toyos-sched/sim/` is the deterministic simulator; `toyos-sched/loom/`
  the model-checking harness.
  **Stage 7a (cutover).** `kernel/src/sched/` is the driver half of spec §4:
  `payload.rs` (`KernelPayload`/`KernelCtx`, the `LeafLock` impl, `TaskHandle`'s
  published counters), `driver.rs` (percpu `CpuSched` slot behind `with_cpu`, the
  pass entry, the asm switch, the idle loop, the trampoline) and `waitqs.rs`.
  `kernel/src/scheduler.rs` survives as the kernel-facing API and nothing else —
  the cross-CPU run queues, the global blocked pool, `KILLED`, `CTX_TRANSITS`,
  `IN_SCHEDULE`, `POISONED`'s scan, `PERCPU_EVENTS`, `EventSource`-keyed wakes and
  `handle_outgoing`'s post-switch parking are gone, and so is the Stage 5 shim
  `kernel/src/waitq.rs`. `Hw` is complete: `KernelHw` gained `switch` + `release`
  with no change to the task-blind half. Wait queues now live on their objects
  (spec §8.6) — `Arc<WaitQueue>` on pipe ends, listeners and io_uring rings;
  statics for keyboard/mouse/net/audio; fixed bucket arrays for futex words and
  for the by-name waits (waitpid/join/sleep, which are woken by `wake_direct` and
  never as a queue). `EventSource` survives only as io_uring's poll key.
  **Stage 7b (balance on, retire by message).** `Env::steal` is `true`: an idle
  pass posts one `StealRequest` to the busiest CPU and a loaded pass answers it
  from surplus, alongside the spawn-time (rotating least-published-load) and
  wake-time placement 7a already had. The two pass entries built identical
  `Env` values, so it now comes from one `driver::env()` that takes the preempt
  guard by reference. `retire_task` no longer spins: it posts `Msg::Retire` and
  parks on a wait queue hung off the target's `TaskHandle`, woken by
  `KernelHw::release`. The wait condition moved with it, from "the state word
  reads `Dead`" to "`Hw::release` has run" — which is what the callers actually
  need, because `Dead` is published by the reaping transition one pass *before*
  the release, so teardown could free pages while the dying CPU still stood on
  that thread's kernel stack. It also fixed a silent accounting loss: `exit()`
  and `kill_process()` call `handle.merge_into()` immediately after retiring,
  and under 7a that read the handle before `finalize()` had written to it.
  Spec §7.6's `notify` field on `Msg::Retire` was deliberately **not** added —
  a running target dies at a later safe point, so a notify riding the message
  would have to be stashed for whichever site eventually kills it, and
  `Hw::release` already *is* that single site, kernel-side, where the wait can
  additionally cover the payload drop. The core keeps one wake path.
  7b changed **nothing measurable** in the audio counters — see the known issue
  below; the recorded prediction that balance would close the smp=8 tail is
  falsified.
  **Scope note:** 7a deleted what the cutover orphaned, because dead code is a
  build error here — it could not leave the legacy body compiled. What is left on
  7c's list is `EventSource` and `source_ready` (io_uring's poll key, not a
  scheduler concept any more) and `Lock::force_unlock`, which now has no caller at
  all and should just go.
  Host tests: `cargo test` inside `toyos-sched/` (unit + loom + sim; ~15 s). The
  harnesses' teeth are proven by negative gates, every one of which is required
  and every one of which is a *port of a shape the kernel actually had*:
  `TOYOS_LOOM_RAW=1 cargo test -p toyos-sched-loom --features no-preempt-guard`
  must FAIL (it does, on invariant I2); the simulator's `old_steal_port`
  scenario — a port of the OLD steal-and-scan algorithm — must fail while the same
  workload passes under the new protocol (it does, on I1/I6 and on I8, the
  address-space-freed-under-a-live-task detector); and the two blocking-shape
  gates below.
  **A block is two steps (2026-07-29).** Spec §10.2 always said "ticket phases are
  separate steps"; the VM did not implement it — `do_block` committed the ticket
  and ran the pass in one step, so the window `8508b37` fixed was outside the step
  relation and the simulator certified a protocol it could not execute. The block
  is now `Step::Exec` (register + re-check) then `Step::BlockPass` (drain, commit,
  park), with `Scenario.block: BlockShape` naming which side of the boundary the
  commit falls on. That adds a **second negative gate**, `old_commit_before_pass`
  (the pre-`8508b37` shape, caught by I1 in every one of 200 seeds, median 3
  steps), and its control `old_commit_fused` — the same shape with the halves
  fused, i.e. the simulator's own pre-split behaviour, which must stay **clean**
  because that is the blind spot itself. §8.1's residual window (a claim landing
  between the commit and the park) cannot be a step boundary — a `SchedPass`
  borrows `CpuSched` — so it is reached by injection and *counted*
  (`Outcome::pre_park_claims`); before this it was zero in every run ever made,
  which meant `RunningTask::park`'s `WakeQueued` arm was dead code. The shrinker
  now also preserves a repro's violation *kind*, so minimizing an I1 trace cannot
  silently hand back an I8 one.
  **The registration window is preempt-off, and that is now modelled (2026-07-29).**
  Splitting the block exposed two live kernel defects, both fixed. (1) An
  involuntary pass between `prepare_wait` and `block_on` aborted on `check_cpu`;
  the window is now held preempt-off by `sched::driver::Ticket`, whose guard *is*
  the blocking pass's bracket. `Vm::enabled` still withholds `Step::Pass` mid-block
  — but now because `Scenario.window: WindowShape` says the kernel holds preemption
  off there, not because the assert would abort the run. That buys a **third
  negative gate**, `old_preemptible_window`, which is the only one that fails by
  *aborting* rather than by a recorded violation, so it runs through
  `explore::run_catching`. (2) A `Retire` landing mid-window was swallowed;
  `WaitTicket::commit` now returns `Commit::Killed` and both drivers exit instead
  of parking, so `Vm::block_pass`'s compensating `kill_pending` arm is gone.
  Both removals are backed by counters rather than by argument
  (`Outcome::killed_at_park`, alongside `pre_park_claims`).
  The full Stage 4 exit criterion
  runs from the CLI, not from `cargo test`:
  `cargo run --release -p toyos-sched-sim -- gate 10000` (10⁴ seeds/scenario) and
  `-- fuzz-sweep 10000000` (10⁷ fuzz steps/scenario).
  Gate A (audio glitch) is **two-tiered** — see Build & test above and the long
  form in `tests/audio-baseline.toml`. The fast tier runs inside every
  `cargo test`; **a stage transition gates on the thorough tier**
  (`cargo test --test toyos-build -- --audio-gate 30`), which is the only one that certifies a
  rate. Both read the same per-config record: the per-run counter ceilings
  (`max_wake_lat_us`, `drains`, `underruns`), the strict zero-gap wav histogram,
  and — new — the recorded 30-run *sample* of each counter, which the thorough
  tier compares a fresh sample against. Every number in that file is justified
  in the file itself. The harness also fails if the capture's silence carries no
  dither — without that check, reverting the §5.4 quantizer fix would silently
  collapse the dropout detector's band and take the instrument out (see below).
  Stage 2 landed: per-CPU `kernel/src/irq_ring.rs`
  carries `(IrqSource, ts)` IRQ-time stamps for audio/net/xHCI; ISRs publish + set
  need_resched via the shared `msix_entry!` macro; `drain_events` consumes the ring
  (no global pending flags, no cpu==0 gating).
- `specs/capability-handles-spec.md` — refcounted kernel objects behind typed
  per-process handles (Fd→Handle); subsumes the SharedToken/io_uring/Fd items below.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one
  wait-free completion primitive, one park/recheck site.

## Known issues

<!-- Track blocking issues and findings here. Remove when resolved. -->
- ~~An involuntary pass between `prepare_wait` and `block_on` panics the kernel.~~ **Fixed 2026-07-29 by closing the window.** The registration window is now preempt-off: `kernel/src/sched/driver.rs`'s `Ticket` raises the preempt count before it reads the current task and holds it until the ticket is cancelled or parked, and `pass_block` no longer takes a bracket of its own — the ticket's guard *is* the pass's, so the window and the pass are one continuous preempt-off region. Why closed rather than tolerated, since `8508b37` tolerated its window: that one is a *remote* CPU acting between two of our own instructions and genuinely cannot be closed. This one's only intruder is our own `preempt::enable` slow path, reached from the guard drop of any lock the re-check takes (and, before the fix, from inside `WaitTicket::cancel`'s own `dequeue` — the cancel path had it too). The alternative — teaching `RunningTask::preempt` to accept `Committing` — is worse than the assert, not better: the `Ready` word it publishes makes every waker that pops the registration report `Claim::Lost` and move on, turning a loud panic into a silent lost wake.
- ~~A `Retire` that lands while its target is between `prepare_wait` and the park is dropped, and the thread is never reaped.~~ **Fixed 2026-07-29 in the core.** `WaitTicket::commit` now checks the sticky kill bit first: if it is set it dequeues, unwinds phase 1 back to `Running(cpu)` and returns the new `Commit::Killed`, which both drivers dispose with `dispose_exit`. Spec §6.3 already listed the park among the safe points and §7.6 already promised a killed task dies at its next one — the kernel simply did not keep the promise there, so the spec needed no change. Putting it in the core rather than in each driver is the point: `Vm::block_pass`'s compensating `kill_pending` arm is deleted, and the sweeps stay clean because the core covers it. Rejected alternative: honouring the kill at *wake* instead, which does nothing for a task nobody ever wakes. Covered by `loom_retire.rs`'s `a_retire_racing_the_park_commit_always_leaves_someone_to_reap` and counted, not assumed, by `Outcome::killed_at_park`.
- **`handle_retire`'s `need_resched` on a running target is a request the next pass may decline.** Noticed while fixing the above, not fixed. `preempt_if_due` fires on quantum expiry or an RT task in the band, and on neither for a merely-killed task — so the pass that `need_resched` asked for can run, clear the request and resume the task, which then dies only at the real quantum end. That is what §7.6 promises ("bounded by the quantum") and `retire_task`'s spin deadline is 100 quanta, so it is conformant rather than broken. Adding `|| current.shared().kill_pending()` to `preempt_if_due` would make the request mean what it says for one atomic load per pass.
- **`io_uring.rs:352-357` can drop an armed wait ticket.** `let ticket = prepare_wait(&queue); if cq_count(ring_id)? >= min_complete` — the `?` propagates out with the ticket still armed, tripping its drop bomb (and now also leaking a preempt level, though the panic gets there first). Pre-existing and probably unreachable today: `cq_count` only errors on an unknown ring id, and the ring was just used. Every other blocking site consumes its ticket on all paths.
- **`audio_tone_load` at smp=1 is flaky on an unmodified tree, so the recorded audio baseline is wrong.** Measured 2026-07-28 during Stage 5: 2 failures in 19 baseline runs (1- and 8-period gaps), with soundd's own underrun count spanning 0–17 across runs that all "passed". `tests/audio-baseline.toml` records all-clean, so Gate A both under-reports real glitches and will fire false failures during the scheduler migration — the one gate the migration most depends on. Two things to settle, in order: (1) is the residual a real single-core underrun or harness noise (soundd reporting non-zero underruns on passing runs says real), and (2) record an honest histogram rather than an aspirational one. Until then, treat a single green audio run as evidence of nothing. Note this does not undo the audio work — the pre-fix baseline was 1,834 gaps — but "0 gaps, verified" was too strong a claim. **Update 2026-07-28:** the dominant contributor was soundd's re-prime path (`specs/audio-glitch-distribution-2026-07-28.md` mode A) and it is now fixed. Re-measured over 30 serial suite invocations per side, 120 config-runs each: 7/30 → 1/30 suites red, 12 → 1 gap events, 464 ms → 29 ms of total mid-tone dropout, and the ×8 quantisation is gone (8 events / 418 ms of multiples-of-8 → zero). The residual is one 10-period gap on `audio_tone_load` smp=1 — a client slot miss driven by the unchanged wake lateness (median ~35 ms against a 23.2 ms pipeline), which is the separate, still-open defect. Whoever re-records the baseline should measure fresh; the numbers in the distribution doc are pre-fix. **Update 2026-07-28 (baseline re-recorded):** measured fresh over 30 serial suite invocations, 120 config-runs, quiet host, post-quantizer-fix. Gap-producing runs, raw counts: `audio_tone` smp=1 **1/30** (one 1-period), `audio_tone` smp=8 **0/30**, `audio_tone_load` smp=1 **2/30** (one 2-period, one 5-period), `audio_tone_load` smp=8 **1/30** (two 4-period). Three of 30 suite invocations red on gaps. The `gaps` histograms stay strictly zero — the bar was not loosened — and Gate A now additionally asserts on soundd's counters, whose distributions are recorded per config in `tests/audio-baseline.toml`. The wake-lateness residual is confirmed and is now measured properly (streaming-scoped): median 13.9 ms but max **93.0 ms = 4.0 pipeline depths** on `audio_tone_load` smp=1, with up to 13 full pipeline drains in one 3 s stream. That statistic is badly heavy-tailed — its 30-run maximum more than tripled between n=15 and n=30 — so `drains` and the zero-gap histogram, not the latency maximum, are what actually hold that config. **Update 2026-07-28 (gate rebuilt, this item is closed as a *baseline* problem):** the baseline is no longer wrong — it records the measured distribution, sample and all. What was still wrong was the *statistic*: one run per config per invocation is a Bernoulli trial against a 3.4% pooled dropout rate, which reds a clean tree on 12.8% of invocations and is blind to a doubling. Gate A is now two-tiered (see Build & test); the fast tier confirms a dropout with a second boot before failing, and stage transitions gate on `--audio-gate 30`, which compares a fresh 30-run sample against the recorded one. Residual, unchanged and still scoped out: 9 of 120 runs wake later than the 23.2 ms pipeline they are feeding, worst 4.0 depths.
- ~~The audio harness's per-test 30 s timeout fired once in 30 quiet-host runs.~~ **Diagnosed and fixed, and it was not a guest stall.** `run_test` matched `===TEST_END ` as a *line prefix*; soundd was mid-`println!` when the runner printed the marker, so the marker landed inside soundd's line and the harness never saw it. The guest had already exited cleanly. The marker is now matched anywhere in the line and the text preceding it is kept (it is usually the stats line gate A reads). Worth remembering as a class: a "guest hang" that only ever appears on the audio tests is more likely to be the shared console than the scheduler.
- **The virtio-console has no line atomicity between writers, and it corrupts machine-readable serial output.** Kernel `log!` output and userspace `println!` interleave *mid-word* into each other's lines: soundd's stats line was split by a kernel message in 1 of 15 runs and by the tone client's own `println!("tone done")` in 2 of 120 config-runs, each time pushing the line's tail onto the following line. `tests/common/audio.rs` now reassembles both cases (strip `[kernel …]` spans; resume a field's digits after the next newline), but that is a reader-side workaround for a writer-side defect — any tool parsing serial output has the same problem. Serial writes of a whole line should be atomic.
- **Gate A records physically impossible counter values as data, and its re-baselining output would poison itself.** Stage 6's thorough tier (`--audio-gate 30`, PASS) contains `iter014: audio_tone smp=1 soundd: wake_lat 153766519us (6622.44 pipelines, limit 56000us)` — 153 *seconds* of wake lateness inside a 3-second test. It cannot be a measurement; it is a counter defect (stale or unset reference in the lateness computation is the obvious candidate, unconfirmed). Three consequences, in increasing severity. (1) The thorough tier applies no per-run ceiling, only distributional comparison — so a value 2745x over its own printed limit did not fail anything. (2) Mann-Whitney is rank-based and therefore robust to exactly this: one absurd outlier at the top of a 30-sample does not move the median, which is a virtue for noise and a hole for instrument faults. (3) Worst — the gate prints toml-ready output intended to become the next baseline, and that output contains `max_wake_lat_us = [..., 24593, 153766519]`. Pasting it in sets the ceiling to 2x 153766519, permanently disabling the wake-latency check. The gate needs an *implausibility* bound distinct from its regression bound: any counter beyond a physical limit (a few pipeline depths) is an instrument fault and must fail loudly, never be ranked and never be recorded. Note the same run also had a real 5-period dropout and `windows 3` where every other run had 2.
- ~~7a is gated red on smp=8 only, and balance (7b) should close it.~~ **Tested at 7b, and the prediction is falsified. The counter ceilings are still red and the cause is not the scheduler's placement.** Same-session A/B, 5 `cargo test -- audio` invocations per side, 7a rebuilt from its own commit in the middle of the 7b session (numbers are per-config first boots). Pooled ceiling breaches: 7a **9 of 20** config-runs, 7b **7 of 20** — indistinguishable at n=5. `audio_tone` smp=8 `wake_lat`: 7a 8979/9053/11663/8346/**59147**, 7b 8654/**61309**/**58896**/8539/**55975** (limit 48000). `audio_tone_load` smp=8 `wake_lat`: 7a 8971/7942/**107776**/**106270**/8439, 7b 10644/8396/**110901**/9588/**109577** (limit 80000). `drains`, `audio_tone_load` smp=8: 7a 7/7/6/5/7, 7b 6/8/7/6/7 (limit 6). Three things this measurement establishes. (1) **The outlier is not multi-CPU.** The same family appears at `-smp 1`: 7a produced `audio_tone_load` smp=1 `wake_lat` **203889 us** and 7b produced **213379 us**, on a machine with no sibling to migrate to. So "a task woken onto a busy CPU cannot migrate" cannot be the mechanism. (2) **The outlier is a first-window, single-sample event that does not track harm.** Reading the raw `soundd: wakes=` lines, every breach is in the `clients=1` window and the following `clients=0` window is always 7–12 ms; the breaching runs mostly report `underruns 0` and an empty gap histogram. Its values cluster at ~55–65 ms, ~103–111 ms and ~200–215 ms — roughly 1:2:4 — which is the shape of a counted quantity, not of a scheduling delay. This is the same class as the recorded 153-second reading: the gate needs an implausibility bound, and the *statistic* is probably instrument-side. (3) **These counters drift across batches on one host with no code change.** The 7a numbers measured at the start of this session (`wakes` ~1030 on `audio_tone` smp=1, `drains` 4/4/4/3/3 on smp=8) and the 7a numbers measured 40 minutes later from the identical commit (`wakes` ~884, `drains` 5/6/4/4/3) do not overlap. Any cross-session comparison of `wakes` or `drains` — including the recorded 30-run baseline sample — is therefore weaker than it looks. Next step is to instrument soundd's lateness computation, not to touch the scheduler. **Mechanism located 2026-07-29 (unfixed):** `userland/soundd/src/main.rs:672` computes `lateness = clock_nanos() - dll.t_estimated`, and the first completion after a client connects is measured against a `t_estimated` established during the *idle* phase — where soundd runs with `timeout = u64::MAX`, purely completion-driven, waking once per whole pipeline (~43/s) rather than once per period. The stats reset at `:660` fires in the same iteration, so the first post-reset sample is taken against a prediction made under different timing. That accounts for every observed property: first-window only, `clients=1` only, 7-12 ms once the DLL re-locks, no correlation with harm. The fix is to not sample lateness until the DLL has re-locked after a client arrives — the same shape as the `underruns` fix, which was also a counter measuring the pre-roll.
- **Gate A's per-run `drains` ceiling is miscalibrated, and its meaning is stale.** It failed Stage 6 on `audio_tone_load` smp=1 with `drains 44 > 26` — on a run whose gap histogram was empty, `underruns` was 0/70, and wake lateness was 1.66 pipelines against a limit of 8.0. A 12-run A/B against the parent commit in the same session showed a max of 2 on the Stage 6 side and 6 on the parent, so the 44 was a non-reproducing outlier, not a regression. Two separate problems. (1) The statistic is heavy-tailed and mostly zero — the recorded sample is `[0×10, 1,1,1, 2×7, 3,4,4,5,5,6,6,7,10,13]` — so a ceiling of 2× a 30-run maximum is itself mostly luck; the gate's author flagged `drains` as having no distributional power for exactly this reason and kept it as a per-run ceiling anyway. (2) More importantly the *semantics* changed underneath it: before the proportional-recovery fix (`91a653c`) every drain meant 23.2 ms of guaranteed silence, so `drains` was a good proxy for harm and `tests/audio-baseline.toml` still calls it "the sharp instrument". That fix decoupled drains from harm deliberately — the failing run is the proof. A per-run failure should require evidence of *harm* (gaps or underruns), with `drains` reported but not fatal. Note `wakes` was 314 against a recorded sample spanning 426–496, which is the stronger anomaly signal and is already collected but ungated.
- **Gate A's fast tier fails on `drains`/`underruns` after the Stage 7a cutover, reproducibly, while the zero-gap histogram is cleaner than the baseline.** Final numbers, measured after the lost-wake fix over 4 `cargo test` invocations (16 config-runs) on a quiet host; the 134 non-audio tests pass every time. `audio_tone_load` smp=1 fails every run on `underruns` 114–122 against a ceiling of 70 (recorded 30-run sample 2–33). `audio_tone_load` smp=8 fails `drains` 8–9 against 6 on two runs in three; `audio_tone` smp=8 fails `drains` 5–6 against 4 on two in three. `max_wake_lat_us` is now usually 7–9 ms (0.3–0.4 pipelines, against a recorded median of 13.9 ms) but is **bimodal** — the same config also produces 100–145 ms outliers. Three things make this hard to read as a plain regression. (1) `underruns` and `drains` are two independent signals. A claim recorded here that they track at 8× (the submit batch) does not reproduce — measured 121/11 and 8/7 across configs — and it was passed through from a report without verification; only `underruns` was defective.md already records `drains` as miscalibrated and semantically stale. (2) soundd's own `wakes` on `audio_tone_load` smp=1 went from a recorded 426–496 to 1050–1130: the CPU hog no longer starves it, which is B7 being fixed, and its mix cycles now arrive *before* the client has refilled — which is what `underruns` counts. (3) The harm measure improved: 16 config-runs produced 3 gap events, against a recorded 3-of-30-suites dropout rate. Three hypotheses were tested and refuted: the run-queue tie-break (a real bug, fixed, changed nothing here), a shorter RT boost window (worse — underruns 137 and an 8-period gap), and removing the client boost entirely (underruns unchanged at 102, but two real gaps appeared, so the boost is load-bearing for audibility and is not what moves this counter). **Update 2026-07-29 — the `underruns` half was a counter defect and is fixed; the `drains` half is not.** `underruns` counted every silent period from the moment a client *connected*, but a client sends `MSG_STREAM_OPEN` before it has any audio, so the whole connect-to-first-period pre-roll was counted as dropout. Proof, arithmetically, from the pre-fix runs: `submitted − 1034 tone periods − underruns` = 187–300 ms on all four configs, i.e. exactly the tone client's own post-tone `sleep(200ms)` of delivered zeros, leaving `underruns` == the pre-roll to the period. It got worse under 7a only because soundd now wakes across that same fixed window ~2.4x more often. soundd now counts a silent period only while some stream `is_streaming()` — latched by its first delivered period, cleared when it asks to close — which leaves a mid-stream stall fully visible. Same-session A/B, 3 `cargo test -- audio` invocations per side: `audio_tone` smp=1 18/18/18 → 2/2/3; smp=8 8/8/11 → 0/0/0; `audio_tone_load` smp=1 **121/106/90 → 43/43/37** (ceiling 70, now passing on every run); smp=8 8/8/12 → 0/0/2. The counter now tracks audible harm 1:1 — one run with wav gaps `[2p 8p 14p]` reported exactly 24 streaming-phase underruns. **Two things remain.** (a) `drains` still breaches on `audio_tone_load` smp=8 (7 vs 6) on 3 of 3 runs and `audio_tone` smp=8 (5 vs 4) occasionally, and the bimodal `max_wake_lat_us` outlier (100–210 ms) still breaches sporadically on both `_load` configs — so the fast tier is still red, now for reasons unrelated to underruns, and the `drains` entry above still applies. (b) `tests/audio-baseline.toml`'s recorded `underruns` **sample** (medians 4–20) is now on the wrong scale, so the thorough tier's Mann-Whitney on it has no power left; it is one-sided in the "worse" direction so it will not false-red, it will simply stop detecting. Re-record it (the prose there describing `underruns` as having a "floor of ~4" from a "stream-start transient" is likewise stale). The `underruns` **ceiling** of 70 was deliberately not touched.
- **The simulator cannot reach the window that produced the only correctness bug the cutover found.** `block_on` used to commit its wait ticket at the call site and then enter the pass; a remote waker claiming the freshly-`Blocked` task posts `Msg::Wake` to the parking CPU, whose own drain then consumed it before the task was in `parked` — wake lost, and the park asserted on a word the waker had advanced. It reproduced on `--smp 8` about twice in five audio suite runs. The sim's `do_block` has the identical shape (`ticket.commit()`, then `run_pass`) and its `interfere` hook fires *before* the commit, so "a wake between the commit and the pass's drain" is not in its step relation at all. Fixed in the kernel by committing inside the pass, after the drain (spec §8.1 says phase 2 belongs to the pass, and this is why). The sim should get the interleaving point — commit and pass as two steps — or it will keep certifying a protocol it cannot execute.
- **`ps`, `stats` and `dump_blocked` lost their cross-CPU view at Stage 7a, and only the last one is visibly worse.** A `CpuSched` is `!Sync` and reachable only from its own CPU, so walking a sibling's queues is now unwritable rather than racy. `task_cpu_ns`/`task_sched_state` were rebuilt on values the owning CPU *publishes* — `TaskHandle`'s counters, republished at each end of a pass, plus the core's rendezvous word — so they are accurate and lock-free (this also closes the old `try_lock`-and-skip misreport). `dump_blocked` has no such substitute: it now prints only the calling CPU's parked map, by `TaskKey` and `WaitClass`, with no process name and no per-source detail, because the pool it used to walk does not exist. A cross-CPU view costs a message round trip; whether the diagnostic is worth building is a diagnostics question, not a scheduler one.
- Profiling tooling is incomplete — Layer 1 (process accounting counters + `stats` tool) is implemented. Layer 2 (event tracing) and Layer 3 (RIP sampling) are not yet built. See Diagnostics roadmap.
- **CORRECTED: the "~half the CPU is unattributed kernel time" entry was wrong, and its sign was backwards.** Investigated 2026-07-29, `specs/cpu-attribution.md`. `stop_cpu_timer` adds *one* delta to both the per-thread `cpu_ns` that `ps` reads and the per-CPU `CPU_TIME_NS` that `SYS_SYSINFO` reports as busy — they are one accumulator, not two measurements. Genuinely unattributed kernel time is therefore absent from **both** numerators and cannot open a gap between them; it pushes the 97% *down*, so true busy **exceeds** 97%. The 45-vs-97 gap is reader-side: `ps` divides a since-thread-creation cumulative by since-boot uptime (`userland/toybox/src/ps.rs:54-56`) while the compositor taskbar computes a correct one-second delta (`userland/compositor/src/main.rs:1512-1518`) — a lifetime average compared against an instantaneous sample — plus per-row flooring via `as u32` (up to a point per row, 12-20 rows) and reaped processes whose time stays in the system total forever but vanishes from every row. The recorded prime suspect was also wrong: `mov cr3` happens *after* `start_cpu_timer`, so the address-space switch is charged to the incoming task, not lost. `ps` already fetches `total_cpu_ns` at sysinfo bytes 32..40 and ignores it; `header total_cpu_ns − Σ(printed cpu_ns)` is exactly the reaped+zombie loss, measurable today with no kernel change. Real unattributed windows *do* exist — the scheduler's pick-and-arm window (deliberate, documented) and the whole idle-loop body, which does substantial work and is counted as idle — but they are a smaller and different problem than the entry claimed.
- **The kernel is built unoptimized, and a release kernel has never been built in this tree.** `kernel/Cargo.toml` has no `[profile]` section, so the dev defaults apply: opt-level 0, debug-assertions, overflow-checks. `kernel/target/x86_64-unknown-none/` contains only `debug/`, and `src/build.rs` makes `--release` opt-in while `cargo test` never passes it. Userland meanwhile sets `opt-level = 2` for every dependency — a lesson learned there (the comment cites rustysynth rendering 2-4x slower than real time at opt-level 0) and never applied to the kernel. Verified in the shipped binary: `clock::nanos_since_boot` calls `rdtsc` **out of line** despite `#[inline]`, plus two out-of-line atomic loads, two overflow-check branches, and a `__udivti3` call. Any kernel-versus-userland CPU comparison is inflated in the kernel's favour by an unknown but large factor, and unlike the TCG distortion this one holds identically on real hardware.
- soundd idles at ~7% of a core (mixing/dithering silence at period rate even with no clients). Acceptable but worth revisiting with the idle policy in spec §5.8. Measured 2026-07-28: with no clients `timeout` is `u64::MAX`, so the mix loop is purely completion-driven and only wakes once the *whole* pipeline has emptied — soundd runs a continuous drain→refill cycle at ~43/s (one every 23.2 ms) instead of a per-period cadence. Harmless (silence over silence) but it is why the old `reprimes` counter was always non-zero and useless as a streaming signal; the counter is now `drains` and gated on having clients.
- **`f32::round()` lowers to a `compiler_builtins` `roundf` call on the ToyOS target, not `roundss`.** The fixed quantizer calls it once per sample (256/period, ~344 periods/s ≈ 88k calls/s). SSE4.1 is universally present on the 2020+ hardware baseline, so enabling it in the target spec would turn this into one instruction; whether to widen the target's feature set is a separate decision. Impact is small and swamped by the debug build's out-of-line `clamp`/iterator calls in the same loop, but it is real per-sample work that should be free.
- **soundd's audio path has several defects found in a spec audit (2026-07-28), none fixed.** In rough severity order. (1) `toyos/src/poller.rs` never bounds SQ occupancy — `poll_add_fd` advances `tail` without checking `head`, so at `sq_size` concurrent registrations the ring overwrites entries and `pending()` grows monotonically; `io_uring_enter` then rejects the batch, the error is discarded with `let _ =`, and the caller spins forever. soundd creates `Poller::new(32)` and registers `1 + clients.len()` polls per iteration, so 32 unprivileged `connect("soundd")` calls permanently wedge its control thread (no accepts, no closes, no client teardown) while the mix thread keeps playing. Violates spec §10. The defect is in the shared SDK, so every `Poller` user inherits it. (2) `CommandRing::push` asserts when full, and the control thread drains *all* buffered client messages before yielding — ~65 `MSG_STREAM_SET_VOLUME` messages in one write trip the assert. Client-triggerable panic. (3) `SYS_SET_RT_PRIORITY` is completely ungated (`kernel/src/arch/syscall.rs`): any process can put any number of threads in the RT band, directly starving soundd's mix thread. Spec §9.4 requires it not be available to unprivileged applications; this is the audio subsystem's core scheduling guarantee with no enforcement behind it. Note this is *not* covered by the `SYS_AUDIO_SUBMIT` item below — that one enumerates device syscalls. (4) A NaN volume from a client propagates through `f32::clamp` (which returns NaN unchanged), NaNs the whole shared mix bus for ~220 frames — global digital silence across all streams — and then permanently, silently mutes that client, uncounted by `underruns`. Spec §7.4 says out-of-range values are clamped. (5) The cpal ToyOS backend hardcodes 44100/2ch/i16 and rejects everything else, so soundd's resampler and channel-conversion paths (spec §6/§8) are unreachable from any real client and effectively untested; it also `assert_eq!`s the device rate against a compile-time constant, so changing the driver's rate aborts every cpal app. (6) soundd never frees the per-client shm region — `SharedMemory::Drop` only unmaps, and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB page for soundd's lifetime. (7) Spec §5.7 crash detection is structurally impossible: soundd holds the pipe's read end itself, so the write can never report a broken pipe. (8) `virtio_sound.rs` queries PCM capabilities, logs them, then calls `configure(44100, 2)` unconditionally and silently remaps unsupported rates to 44100 (spec §9.1–§9.3 not implemented; silent degradation). (9) The §5.3/§5.10 "wait until clients have filled" condition does not exist — the wait ends on the first DMA completion, which is an unrelated event. (10) The two TPDF dither draws are not independent (the second is a deterministic function of the first), a literal §5.4 non-conformance whose practical effect was not measured. Lower severity: decode divides by 32768 while quantize multiplies by 32767, so the no-conversion passthrough path is not unity gain; `Poller::wait` silently drops all non-positive completions; `AudioInfo::as_bytes` copies 6 bytes of uninitialised kernel stack padding to userspace; unknown audio device command bytes report success and do nothing.
- **A thread retired while parked leaves its node in the wait queue forever.** `Msg::Retire` reaps a `BlockedTask` on its home CPU; the `Registration` that would have dequeued it lives on the dead thread's own stack and is never dropped (the kernel does not unwind), so the queue keeps an `Arc<TaskShared>` for a task that is `Dead`. It is a leak, not a correctness hole — `claim_wake` on a dead task returns `Claim::Lost` and `wake_one` moves to the next waiter, which is exactly what §8.2's retry arm exists for — but the list grows across process kills and a `wake_all` walks the corpses. The fix belongs with the intrusive `wait_node` the core still owes (`waitq.rs` holds waiters in a `VecDeque` instead, see its module note): with an embedded node, `reap` could unlink it.
- One unreproduced observation: `ps` appeared to stall for >2s under heavy single-core load (later runs fine). If seen again, capture with LLDB before restarting.
- A panic *while holding* `PROCESS_TABLE` hangs the panicking CPU: `try_recover_from_panic` lands in `sched::driver::idle_loop`, whose `reap_poisoned` takes that lock unconditionally every iteration, and the dead thread never releases it. Pre-existing and unchanged by the panic-recovery fix (a `try_lock` could not have saved it either — a spinlock's `try_lock` fails for its own holder too). The general shape — locks a dead thread can strand — belongs to the capability-handles/ownership work.
- **A panic that needs the allocator lock the panicking thread holds produces no output at all.** `mm/alloc.rs:12`'s >2 MiB assert fires inside `KernelPageSource::alloc`, which runs while `KernelAllocator::alloc` holds `self.dlmalloc.lock()` (alloc.rs:78). The *reporting* half of the panic path is already allocation-free and correct — `log!`, `crash_report`, symbolization, both backtraces, `panic_flush` all use fixed/static buffers. The damage has two separate causes. (1) `crash_report` writes into the 64 KiB static log ring, and the only drains are `panic_flush` (called from `apic::halt_all_cpus`) and opportunistic `try_lock` drains; on the *syscall* path `main.rs:139` branches into `try_recover_from_panic` → `cpu_idle_loop` and never reaches `halt_all_cpus`, so the ring is never drained. (2) the idle loop then allocates or frees (watcher-list `Vec` clones in `net.rs`/`audio.rs`, the mailbox drain's `Box`ed task records, BTreeMap frees) and spins on the held lock — `dealloc` takes the same lock, so a free is as fatal as an alloc. The reentry guard cannot help: a spin is not a panic, and `main.rs:140` disarms the guard before recovery anyway. Cheapest real fix is one line — call `panic_flush()` at `main.rs:135`, right after `crash_report` and before the recovery branch, mirroring the early-panic branch at `main.rs:108`. A `Lock` escape hatch is also cheap: `force_unlock` already exists (sync.rs:98) and `PANIC_DEPTH` (main.rs:77) is already a GS-independent per-CPU panic flag to key off. There is no `#[alloc_error_handler]`, so a null return from dlmalloc wedges identically with a worse message. **This is its own task — do not fold it into an unrelated commit.**
- **Untrusted-input panics that remain (audited 2026-07-28, none fixed).** One syscall, no setup: `sys_mmap(size=0)` and `sys_alloc_shared(0)` reach `assert!(count > 0)` at pmm.rs:263 (`align_2m(u64::MAX)` also wraps to 0); `SYS_NIC_RX_DONE` with no prior poll hits `.expect` at virtio_net.rs:142; `SYS_TLS_ALLOC_BLOCK` has six panics keyed on a raw `module_id` (syscall.rs:1363-1416); io_uring's CQ-overflow `assert!` at io_uring.rs:186 reads a `head` the process writes, so storing `head=1` with `tail=0` panics the kernel; `sys_alloc_shared` of any size larger than free memory hits `.expect` at shared_memory.rs:120; `SYS_THREAD_SPAWN` underflows `stack_ptr - stack_base` at process.rs:726. Accumulation: `SYS_GRANT_SHARED` grows an unbounded `Vec<Pid>` (shared_memory.rs:185), `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op so VA exhaustion hits `.expect` at syscall.rs:1262. **A crafted ELF is a separate and much larger surface** — `DT_STRSZ`/`DT_SYMTAB`/`shnum*shentsize` flow unvalidated into `Vec::with_capacity` and `HashMap::with_capacity` (loader.rs:285/479/594, elf.rs:650/707), `vaddr_to_file_offset` panics outright (loader.rs:279), every `KernelSlice` offset in `load_shared_lib` is `<tag vaddr> - vaddr_min` with no range check (elf.rs:972-994), and `PT_TLS` copies `filesz` bytes into a `memsz` buffer with no `filesz <= memsz` check (loader.rs:747) — that last one is a heap buffer overflow, not just a panic.
- **Missing ownership checks are a class, not an instance.** `device.rs` records five owner PIDs but, until `device::is_owner` was added for `SYS_GPU_SET_RESOLUTION`, nothing outside `release` ever read them. Still ungated, in rough order of damage: `SYS_PIPE_OPEN`/`SYS_SOCKET_CREATE`/`SYS_PIPE_MAP` (PipeIds are small sequential integers — any process can attach to any other process's pipe or socket and read or inject, and `PIPE_MAP` hands it the raw 2 MiB ring page); the NIC set `SYS_NET_SEND`/`SYS_NIC_TX`/`SYS_NIC_RX_POLL`/`SYS_NIC_RX_DONE` (frame injection, packet theft, and `SYS_NIC_TX`'s unbounded length is handed to the device as a descriptor length, i.e. an arbitrary kernel-memory infoleak onto the wire); `SYS_GPU_PRESENT`/`SYS_GPU_SET_CURSOR`/`SYS_GPU_MOVE_CURSOR`/`SYS_SET_SCREEN_SIZE`; `SYS_AUDIO_SUBMIT`; `SYS_SET_KEYBOARD_LAYOUT`; `SYS_LISTEN` (no namespace — first process to claim a well-known name impersonates that service); `SYS_GRANT_SHARED` (any grantee can re-grant, target PID unvalidated, no revoke). Each is now a one-line `device::is_owner` gate or the equivalent, but they need a discussion about which of them should instead fall out of capability handles.
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
- **`bootstrap-cc` is alive but not wired in.** Settled 2026-07-28: it is *not* dead code. It is the first link of a deliberate bootstrap chain — `toyos-cc` (ToyOS's own minimal C compiler, in Rust) compiles TinyCC, TinyCC compiles progressively more capable C, and the chain is meant to end at a working C++ compiler hosted on ToyOS. That is a self-hosting goal, so it does not need a caller today to justify existing. What it does need: nothing references it (no build code, no `system.toml` entry, no test) and it is excluded from the userland workspace, so it silently rots — a chain link nobody compiles is not a chain. It also inherits `userland/rust-toolchain.toml`, so a bare `cargo check` in its directory cross-compiles a host-only tool to the ToyOS target and fails in `ring`/`getrandom`; it must be built with `--target <host>`. Its TinyCC download is https but unverified — repo.or.cz serves an on-demand cgit snapshot whose gzip wrapper is probably not byte-reproducible, so pinning a checksum needs a stable release tarball first. Wire it into the build so it is at least compiled, fix the toolchain inheritance, and pin the download.
- **The `memmap2` fork is 165 lines of unreachable code.** `rust/compiler/rustc_data_structures/src/memmap.rs` cfg-gates `target_os = "toyos"` to a `Vec<u8>` implementation at all 8 sites, and userland lists memmap2 under `[patch.unused]` — so no ToyOS code path calls any memmap2 API. `src/toyos.rs` is compiled and never called; the fork's only load-bearing content is the `0.9.10 → 0.2.1` version relabel that satisfies rustc's pin. Either delete `src/toyos.rs` and let `stub.rs` serve, or drop the toyos gate in `rustc_data_structures` (the only two APIs rustc uses, `map_copy_read_only` and `map_anon`, are correct in the fork). Exactly one of the two should exist. Three real bugs in that module were found and fixed 2026-07-28 — see `forks.toml`.
