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
- **Never `git commit --amend`, `git rebase`, or any history rewrite.** Other agents commit to this tree concurrently; an amend has already silently rewritten another agent's commit (recovered from the reflog, but only because it was noticed immediately). Always add a new commit.
- **Do not poll a background job.** A `Bash` call with `run_in_background: true` sends a completion notification when the command exits — launch it and stop, rather than re-reading the log to find out whether it finished. Polling a 20-minute measurement burns a turn per check and learns nothing. If you must wait on a *condition* rather than a process exit, background ONE command that blocks and then exits (`until [ -f done ]; do sleep 10; done`) — that is a single notification.
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
  and smp=8 and is gated on **two** instruments recorded per config in
  `tests/audio-baseline.toml` — the wav underrun histogram (all-clean = strict,
  unchanged) *and* ceilings on soundd's own counters (`max_wake_lat_us`,
  `drains`, `underruns`). The wav samples ~1100 device periods once per run and
  only fires when a dropout lands inside the tone, so it is a rare-event
  detector; the counters are non-zero on essentially every run and are the half
  with statistical power. Every number in that file is justified in the file
  itself, measured over 30 serial runs per config on a quiet host. The harness
  also fails if the capture's silence carries no dither — without that check,
  reverting the §5.4 quantizer fix would silently collapse the dropout
  detector's band and take the instrument out (see below). Stage 2 landed: per-CPU `kernel/src/irq_ring.rs`
  carries `(IrqSource, ts)` IRQ-time stamps for audio/net/xHCI; ISRs publish + set
  need_resched via the shared `msix_entry!` macro; `drain_events` consumes the ring
  (no global pending flags, no cpu==0 gating).
- `specs/capability-handles-spec.md` — refcounted kernel objects behind typed
  per-process handles (Fd→Handle); subsumes the SharedToken/io_uring/Fd items below.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one
  wait-free completion primitive, one park/recheck site.

## Known issues

<!-- Track blocking issues and findings here. Remove when resolved. -->
- **`audio_tone_load` at smp=1 is flaky on an unmodified tree, so the recorded audio baseline is wrong.** Measured 2026-07-28 during Stage 5: 2 failures in 19 baseline runs (1- and 8-period gaps), with soundd's own underrun count spanning 0–17 across runs that all "passed". `tests/audio-baseline.toml` records all-clean, so Gate A both under-reports real glitches and will fire false failures during the scheduler migration — the one gate the migration most depends on. Two things to settle, in order: (1) is the residual a real single-core underrun or harness noise (soundd reporting non-zero underruns on passing runs says real), and (2) record an honest histogram rather than an aspirational one. Until then, treat a single green audio run as evidence of nothing. Note this does not undo the audio work — the pre-fix baseline was 1,834 gaps — but "0 gaps, verified" was too strong a claim. **Update 2026-07-28:** the dominant contributor was soundd's re-prime path (`specs/audio-glitch-distribution-2026-07-28.md` mode A) and it is now fixed. Re-measured over 30 serial suite invocations per side, 120 config-runs each: 7/30 → 1/30 suites red, 12 → 1 gap events, 464 ms → 29 ms of total mid-tone dropout, and the ×8 quantisation is gone (8 events / 418 ms of multiples-of-8 → zero). The residual is one 10-period gap on `audio_tone_load` smp=1 — a client slot miss driven by the unchanged wake lateness (median ~35 ms against a 23.2 ms pipeline), which is the separate, still-open defect. Whoever re-records the baseline should measure fresh; the numbers in the distribution doc are pre-fix. **Update 2026-07-28 (baseline re-recorded):** measured fresh over 30 serial suite invocations, 120 config-runs, quiet host, post-quantizer-fix. Gap-producing runs, raw counts: `audio_tone` smp=1 **1/30** (one 1-period), `audio_tone` smp=8 **0/30**, `audio_tone_load` smp=1 **2/30** (one 2-period, one 5-period), `audio_tone_load` smp=8 **1/30** (two 4-period). Three of 30 suite invocations red on gaps. The `gaps` histograms stay strictly zero — the bar was not loosened — and Gate A now additionally asserts on soundd's counters, whose distributions are recorded per config in `tests/audio-baseline.toml`. The wake-lateness residual is confirmed and is now measured properly (streaming-scoped): median 13.9 ms but max **93.0 ms = 4.0 pipeline depths** on `audio_tone_load` smp=1, with up to 13 full pipeline drains in one 3 s stream. That statistic is badly heavy-tailed — its 30-run maximum more than tripled between n=15 and n=30 — so `drains` and the zero-gap histogram, not the latency maximum, are what actually hold that config.
- **The audio harness's per-test 30 s timeout fired once in 30 quiet-host runs** (`audio_tone` smp=8, which is otherwise the healthiest config: 0/30 gaps, median 7.4 ms wake lateness). The tone itself is ~3.5 s, so something stalled for an order of magnitude longer than the whole test. Not reproduced and not diagnosed. If it recurs, keep the QEMU alive and attach LLDB — this smells like the same rare class as the unreproduced `panic_recovery` wedge.
- **The virtio-console has no line atomicity between writers, and it corrupts machine-readable serial output.** Kernel `log!` output and userspace `println!` interleave *mid-word* into each other's lines: soundd's stats line was split by a kernel message in 1 of 15 runs and by the tone client's own `println!("tone done")` in 2 of 120 config-runs, each time pushing the line's tail onto the following line. `tests/common/audio.rs` now reassembles both cases (strip `[kernel …]` spans; resume a field's digits after the next newline), but that is a reader-side workaround for a writer-side defect — any tool parsing serial output has the same problem. Serial writes of a whole line should be atomic.
- Profiling tooling is incomplete — Layer 1 (process accounting counters + `stats` tool) is implemented. Layer 2 (event tracing) and Layer 3 (RIP sampling) are not yet built. See Diagnostics roadmap.
- **~half the CPU is unattributed kernel time on single-core under load.** With the Doom demo on `--smp 1`, per-process CPU sums to ~45% while the core runs ~97% busy. Prime suspect: context-switch volume (audio cycles + game + compositor ≈ 1000+ switches/s) with address-space switches, expensive under TCG. Quantify (needs Layer 2 tracing or a switch counter) before optimizing; this is the main obstacle to full-speed single-core Doom.
- soundd idles at ~7% of a core (mixing/dithering silence at period rate even with no clients). Acceptable but worth revisiting with the idle policy in spec §5.8. Measured 2026-07-28: with no clients `timeout` is `u64::MAX`, so the mix loop is purely completion-driven and only wakes once the *whole* pipeline has emptied — soundd runs a continuous drain→refill cycle at ~43/s (one every 23.2 ms) instead of a per-period cadence. Harmless (silence over silence) but it is why the old `reprimes` counter was always non-zero and useless as a streaming signal; the counter is now `drains` and gated on having clients.
- **`f32::round()` lowers to a `compiler_builtins` `roundf` call on the ToyOS target, not `roundss`.** The fixed quantizer calls it once per sample (256/period, ~344 periods/s ≈ 88k calls/s). SSE4.1 is universally present on the 2020+ hardware baseline, so enabling it in the target spec would turn this into one instruction; whether to widen the target's feature set is a separate decision. Impact is small and swamped by the debug build's out-of-line `clamp`/iterator calls in the same loop, but it is real per-sample work that should be free.
- **soundd's audio path has several defects found in a spec audit (2026-07-28), none fixed.** In rough severity order. (1) `toyos/src/poller.rs` never bounds SQ occupancy — `poll_add_fd` advances `tail` without checking `head`, so at `sq_size` concurrent registrations the ring overwrites entries and `pending()` grows monotonically; `io_uring_enter` then rejects the batch, the error is discarded with `let _ =`, and the caller spins forever. soundd creates `Poller::new(32)` and registers `1 + clients.len()` polls per iteration, so 32 unprivileged `connect("soundd")` calls permanently wedge its control thread (no accepts, no closes, no client teardown) while the mix thread keeps playing. Violates spec §10. The defect is in the shared SDK, so every `Poller` user inherits it. (2) `CommandRing::push` asserts when full, and the control thread drains *all* buffered client messages before yielding — ~65 `MSG_STREAM_SET_VOLUME` messages in one write trip the assert. Client-triggerable panic. (3) `SYS_SET_RT_PRIORITY` is completely ungated (`kernel/src/arch/syscall.rs`): any process can put any number of threads in the RT band, directly starving soundd's mix thread. Spec §9.4 requires it not be available to unprivileged applications; this is the audio subsystem's core scheduling guarantee with no enforcement behind it. Note this is *not* covered by the `SYS_AUDIO_SUBMIT` item below — that one enumerates device syscalls. (4) A NaN volume from a client propagates through `f32::clamp` (which returns NaN unchanged), NaNs the whole shared mix bus for ~220 frames — global digital silence across all streams — and then permanently, silently mutes that client, uncounted by `underruns`. Spec §7.4 says out-of-range values are clamped. (5) The cpal ToyOS backend hardcodes 44100/2ch/i16 and rejects everything else, so soundd's resampler and channel-conversion paths (spec §6/§8) are unreachable from any real client and effectively untested; it also `assert_eq!`s the device rate against a compile-time constant, so changing the driver's rate aborts every cpal app. (6) soundd never frees the per-client shm region — `SharedMemory::Drop` only unmaps, and nothing calls `destroy()`, so each open/close cycle strands a 2 MiB page for soundd's lifetime. (7) Spec §5.7 crash detection is structurally impossible: soundd holds the pipe's read end itself, so the write can never report a broken pipe. (8) `virtio_sound.rs` queries PCM capabilities, logs them, then calls `configure(44100, 2)` unconditionally and silently remaps unsupported rates to 44100 (spec §9.1–§9.3 not implemented; silent degradation). (9) The §5.3/§5.10 "wait until clients have filled" condition does not exist — the wait ends on the first DMA completion, which is an unrelated event. (10) The two TPDF dither draws are not independent (the second is a deterministic function of the first), a literal §5.4 non-conformance whose practical effect was not measured. Lower severity: decode divides by 32768 while quantize multiplies by 32767, so the no-conversion passthrough path is not unity gain; `Poller::wait` silently drops all non-positive completions; `AudioInfo::as_bytes` copies 6 bytes of uninitialised kernel stack padding to userspace; unknown audio device command bytes report success and do nothing.
- `ps` state column (`task_sched_state`) still uses try_lock and skips `outgoing` — can transiently misreport under load (display-only; CPU-time column is accurate).
- One unreproduced observation: `ps` appeared to stall for >2s under heavy single-core load (later runs fine). If seen again, capture with LLDB before restarting.
- A panic *while holding* `PROCESS_TABLE` hangs the panicking CPU: `try_recover_from_panic` lands in `cpu_idle_loop`, which takes that lock unconditionally every iteration, and the dead thread never releases it. Pre-existing and unchanged by the panic-recovery fix (a `try_lock` could not have saved it either — a spinlock's `try_lock` fails for its own holder too). The general shape — locks a dead thread can strand — belongs to the capability-handles/ownership work.
- **A panic that needs the allocator lock the panicking thread holds produces no output at all.** `mm/alloc.rs:12`'s >2 MiB assert fires inside `KernelPageSource::alloc`, which runs while `KernelAllocator::alloc` holds `self.dlmalloc.lock()` (alloc.rs:78). The *reporting* half of the panic path is already allocation-free and correct — `log!`, `crash_report`, symbolization, both backtraces, `panic_flush` all use fixed/static buffers. The damage has two separate causes. (1) `crash_report` writes into the 64 KiB static log ring, and the only drains are `panic_flush` (called from `apic::halt_all_cpus`) and opportunistic `try_lock` drains; on the *syscall* path `main.rs:139` branches into `try_recover_from_panic` → `cpu_idle_loop` and never reaches `halt_all_cpus`, so the ring is never drained. (2) `cpu_idle_loop` then allocates or frees (`net.rs:38`/`audio.rs:158` watcher-list `Vec` clones, `WokenBatch`'s `Vec<TaskCtx>` at scheduler.rs:415, `process.rs:573`, BTreeMap frees) and spins on the held lock — `dealloc` takes the same lock, so a free is as fatal as an alloc. The reentry guard cannot help: a spin is not a panic, and `main.rs:140` disarms the guard before recovery anyway. Cheapest real fix is one line — call `panic_flush()` at `main.rs:135`, right after `crash_report` and before the recovery branch, mirroring the early-panic branch at `main.rs:108`. A `Lock` escape hatch is also cheap: `force_unlock` already exists (sync.rs:98) and `PANIC_DEPTH` (main.rs:77) is already a GS-independent per-CPU panic flag to key off. There is no `#[alloc_error_handler]`, so a null return from dlmalloc wedges identically with a worse message. **This is its own task — do not fold it into an unrelated commit.**
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
