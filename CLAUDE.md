# ToyOS

A production-grade operating system built from scratch in Rust. Targets modern x86-64 hardware (2020+), UEFI only. ARM64 planned — keep architecture portable.

The name has no meaning. This is not a hobby project. The quality bar is the same as shipping software: correct, efficient, minimal, zero technical debt. The codebase is regularly scrutinized and refactored. Nothing is sacred except the principles below.

## Principles

- **Zero legacy.** No backwards compatibility. No fallbacks. No workarounds. No BIOS. No 32-bit. Research state-of-the-art OS design instead of replicating what older OSes do. We have no legacy to maintain — exploit that.
- **Zero technical debt.** Every feature is scrutinized. Dead code is deleted. Every abstraction earns its place.
- **Fail fast.** Panics over silent degradation. Exhaustive matches with panics for unexpected values. Never mask bugs. If something is unimplemented, the system screams and dies loudly.
- **Simplicity.** Prefer the simpler solution unless the complex one brings >2x improvement. We can simplify aggressively because we have no legacy constraints.
- **Rust is first class.** Not POSIX. Not C. The entire OS is Rust-native. C-isms tolerated only when the Rust alternative adds no safety or value. Prefer compile-time safety: unrepresentable > checked at runtime > covered by tests.
- **Development ergonomics above all.** The ability to iterate fast matters more than feature count. Tooling comes first.
- **Self-hosting.** The north star is building ToyOS from within ToyOS. No LLVM dependency. Cranelift as codegen backend.
- **Efficient.** Never hog resources without purpose. Free memory when not used. Minimize kernel overhead. The OS must be fast and responsive. General improvements only — never optimize for one specific app. **Hard bar: every component at most 2× the cost of current production OSes** (Linux, macOS, Windows). Note TCG cannot measure this — the distortion is non-uniform (measured 1.06×–6.5× by operation), so only real hardware or same-session A/B counts.
- **No slop comments.** Never add comments that restate what the code does. No "auto-closes on drop", "returns the value", "loop through items". Comments explain *why*, not *what*. If the code needs a *what* comment to be understood, rewrite the code. If a comment reads like a commit message — evidence, measurements, what the code used to do — it belongs in the commit message.

## Architecture

> The architecture is under active development. Details here are a snapshot — always read the code for current state.

**Kernel** — Minimal. New additions to the kernel must be discussed and justified. Currently handles: resource management, scheduling, process lifecycle, filesystem, device arbitration.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel, maps hardware buffers into its own memory, and serves clients over IPC. Crash a daemon, the kernel is fine.

**IPC** — Named services via listen/accept/connect. Pipes backed by shared-memory ring buffers.

**Memory** — 2MB pages only. Demand paging. Shared memory across processes.

**Processes** — PIE binaries. Spawn-based. Full SMP. dlopen/dlsym for shared libraries.

**Scheduling** — Efficient, event-driven, fair-share. Per-CPU run queues. Must scale to 128+ cores without excessive overhead.

**Filesystem** — VFS with mount points. Initrd, tmpfs, NVMe. **The kernel never formats a disk it was not given.** `bcachefs_adapter::probe` reads block 0: a bcachefs superblock is ours and gets mounted, a designation stamp (magic + that device's block count, written by `create_sparse`) authorises a format, anything else is `Foreign` and is never written to — `/home` falls back to a tmpfs. A failed mount is not consent; treating it as consent is what would have reformatted the T14's disk on its second boot.

**Input** — One held-set and one button-merge for the whole machine (`keyboard::handle_key`, `mouse::handle_motion`), so two keyboards or two pointers compose rather than contradict. USB HID over xHCI and PS/2 over the i8042 both feed it in HID usage codes; wire decoding is `toyos-ps2/`. Pin interrupts arrive through `drivers/ioapic.rs`, everything else is MSI-X.

**Syscall ABI** — Defined in `toyos-abi/`. The ABI is the contract between kernel and userland. Includes struct layouts, syscall numbers, constants, and typed syscall wrappers. Completely unstable — read the code for current state. Never add or change a syscall without discussion. The `toyos/` crate builds on top with typed handles, IPC framing, and service helpers — userland code uses `toyos`, kernel uses `toyos-abi` only.

**Kernel must never crash from userland.** A buggy userland process must not be able to bring down the kernel. But if the kernel itself has a bug, it must crash loudly so we can fix it and harden.

Corollary: **fail-fast is for kernel bugs, not for untrusted input.** An `expect()` on a value that crossed the trust boundary is a userland-triggered kernel panic wearing fail-fast's clothes. Untrusted input that cannot be satisfied returns `SyscallError::{ResourceExhausted, InvalidArgument}` — never a panic, and never a silent truncation of the request to make it fit. Three bounds enforce this today: `user_ptr::MAX_USER_STR` (64 KiB, on the primitive, sized so the *derived* allocations stay under the 2 MiB ceiling), `fd.rs`'s `MAX_FDS` (1024, inside `FdTable`'s only two insert primitives), and `elf::MAX_ELF_ALLOC` (2 MiB − 4 KiB, one page of headroom over dlmalloc's granule rounding); the bootloader adds `MAX_ESP_FILE` (1 GiB). All are policy, not physics.

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
- `cargo run -- --gop` boots a UEFI GOP display (`-vga std`) instead of virtio-gpu: the config where the on-screen panic console renders.
- `cargo run -- --metal-sim` boots the T14's hardware shape: GOP + NVMe + xHCI + i8042, no virtio device, no USB HID — with a 16550, so it is fully drivable and the input tests run on it. `--mute` takes the serial away, the T14's literal shape; one test uses it (`screen_panic_muted`). **Agents verify through `cargo test`, never by launching `cargo run`** — the run path opens a QEMU window on the owner's desktop by design; the harness passes `-display none`.
- The harness has one more machine shape than the CLI: `Profile::MetalUsb` is metal-sim with six devices on the xHCI (two keyboards), the config `xhci_many_devices` and `xhci_slot_exhaustion` run on. QEMU cannot starve a controller's slot count, so the latter uses the `xhci-one-slot` kernel feature as its actuator.
- `system.toml` defines which programs to build and the init sequence.

**Gate A (audio) has two tiers.** `tests/audio-baseline.toml` documents both in full and justifies every number in it.

- **Fast** — part of every `cargo test`. One boot per config. Certifies the per-run counter ceilings, that the instrument is alive, and that audio does not *reproducibly* drop out (a dropout re-boots once; only a second one fails). It certifies nothing about a rate — one run is one sample.
- **Thorough** — `cargo test --test toyos-build -- --audio-gate N`. N iterations of all four configs; every per-run outcome becomes a rate or a distribution, compared against the recorded sample by Mann-Whitney (counters) and Fisher exact (yes/no outcomes). **A scheduler-migration stage transition gates on this tier.** At N=30 it detects a 25% shift in wake lateness and a 5% drop in soundd's wake count 99.9% of the time, with a 0.25% false-red rate on a clean tree; it does *not* detect a doubling of the dropout rate, and no N a human waits for would.

The gate's four instrument defects and the dropout regression they hid are closed and written up in `specs/audio-gate-history.md`. The reusable lesson: these counters drift between batches on one host with no code change, so only same-session A/B numbers mean anything.

**A device's size is a shape dimension.** `Profile::MetalDisk` gives the guest the T14's exact 244 GB namespace; the image is sparse (`File::set_len`), so it costs 7.5 MB. Every profile declares its `nvme_bytes` or does not compile. Ask any device for its real capacity, not a token one — `specs/device-test-strategy.md`.

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

**On-screen panic console** (`kernel/src/drivers/panic_console/`) — fatal panics paint the log-ring tail as an 8x16 text grid on the GOP framebuffer; recovering panics never paint; the six boot phase boundaries repaint so a wedge shows the last phase reached. Armed before `serial::init`, takes no lock of any kind. `cargo test -- screen` decodes the screendump glyph-by-glyph against the same `font8x16.bin` the kernel blits; regenerate it with `cargo run -- --regen-font`. **These are the only tests that read pixels** — everything else asserts on console text, because a screenshot is a poor way to ask whether the right process came up.

**QMP** (QEMU Machine Protocol) — Socket at `/tmp/toyos-qmp.sock`. Script at `.claude/qmp.py`:
- `python3 .claude/qmp.py "ls /bin"` — type string + Enter
- `python3 .claude/qmp.py --raw ret` — single key
- `python3 .claude/qmp.py --raw n --ctrl` — Ctrl+N
- `python3 .claude/qmp.py --screenshot /tmp/toyos-screen.png` — capture screen

## Workflow

- Stay focused on the current task. Record findings and issues in `specs/known-issues.md`, don't go fix them — a separate agent will handle it. Add a one-line summary here only if a future agent must not miss it.
- After each task, audit CLAUDE.md and update if the architecture or project state changed. **Keep this file under ~200 lines**: it loads into every session and every subagent. Detail belongs in `specs/`, resolved narrative in `git log`.
- If something is blocking, stop and report it. Don't work around it.
- Never degrade audible or visual quality — even temporarily, even for a big win elsewhere — without the owner's explicit sign-off. Quality tradeoffs are the owner's call.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce output. If a command produces a lot of output or takes long, run it in the background — background tasks automatically get their output written to a file.
- Host tests outside the QEMU suite: `cargo test` inside `toyos-sched/` and inside `toyos-ps2/`.
- **`cargo test` and `cargo run` produce large output** (std rebuild warnings, initrd listing, serial output). Always run them in the background so the Bash tool doesn't silently truncate the output — `... [N characters truncated] ...` in tool output means data was lost. Read the output file afterward. **ToyOS is fast** — full boot is under a second and incremental builds usually finish in seconds, so never assume slowness.
- **Always be empirical.** Never assume a command succeeded or failed — read the actual output. Never assume code works — run it. Never guess at root causes — investigate. Guessing is unproductive; verify everything.
- **Any number in a comment, commit message, or spec must come from a command that was actually run.** If a figure is an estimate, or a bound from a datasheet, it says so. A plausible invented number is worse than no number: the next reader has no reason to doubt it, and it survives every review that only checks whether the prose reads sensibly. Real case: an IST1 stack measurement was written as 5936/2216 before anything was run; the true figures were 9968/4512, and nothing about the invented pair looked wrong. The estimate it replaced had been off by 4x for months and was load-bearing for a proposed fix that would have shipped broken.
- **Never `git commit --amend`, `git rebase`, or any history rewrite.** Other agents commit to this tree concurrently; an amend has already silently rewritten another agent's commit (recovered from the reflog, but only because it was noticed immediately). Always add a new commit.
- **Wait in the FOREGROUND; do not tight-poll a background job.** Background-task notifications work reliably for the *main* agent, but spawned subagents do not stop cleanly when they background work and wait — observed repeatedly, cause unknown. That is why polling looks necessary to them. It is not. Run long commands in the foreground with an explicit `timeout` (default is only 120000 ms; the max is 600000). For work exceeding 10 minutes, background it once and block with two or three long foreground waits on a marker file (`until [ -f done ]; do sleep 20; done`, or a FIFO `cat`) — never hundreds of short polls. Note this is the opposite of what the main agent's tooling advises; subagents differ. Always poll before sleeping: try reading output immediately, and only add a short delay if the poll came back empty.
- **The git index is shared between concurrently committing agents — treat every commit as a race.** Never `git add -A`/`git add .`; stage explicit paths, check `git diff --cached --name-only`, and prefer `git commit -- <paths>`, which commits only those paths and leaves anyone else's staged work alone (applies inside the forks too). **Put the `add` and the `commit` in one tool call**: narrow hunks shrink the blast radius but the window between them is real, and another agent's bare `git commit` has taken a staged hunk from `tests/toyos.rs` twice. **When a change adds a new file and edits a shared file that references it, commit the new file first, on its own** — callee before caller, for commits as well as edits; a sweep that takes the referencing half and leaves the defining half behind gives you a HEAD that compiles neither. **A swept hunk is fixed forward, never undone** — undoing is history rewriting, which is forbidden anyway and is strictly worse than a wrong commit message.
- **Concurrent measurement is unreliable.** Another agent building or booting QEMU in the same tree perturbs timings and shares the cargo target lock. When measuring a flaky test's rate, A/B against the same HEAD in the same session rather than comparing to a number someone recorded earlier.
- **`'rustc' is not installed for the custom toolchain 'toyos'` is another agent's build, not a broken toolchain.** `rustup toolchain link` unlinks and recreates the symlink rather than replacing it atomically, and `toolchain::ensure` called it on *every* build — so any concurrent `rustc` proxy landing in that window died. One agent lost eleven consecutive `cargo test` runs over ~15 minutes while `RUSTUP_TOOLCHAIN=toyos rustc --version` succeeded 20/20 between attempts; that asymmetry is the tell. The link is now skipped when it already points at the right stage2, which removes the window — but **wait and re-run rather than retrying into it, and never try to repair or force-rebuild the toolchain**: that makes it worse for everyone.
- **A feature-carrying test failing as though its feature were absent, while unrelated tests pass, is a clobbered `kernel/target` — not your diff.** Another builder rebuilt the kernel binary between your feature build and your boot, so the actuator never armed. Five tests went red at once this way, with five different features and five different owners: `xhci_slot_exhaustion` reporting `blocks=64` on an `xhci-one-slot` build, `screen_early_panic` timing out waiting for a panic, `i8042_quarantine` reporting the fault was never armed, `va_exhaustion` fitting 1885 mappings into a 256 MiB arena. **Re-run it in isolation before believing it** — the failure is indistinguishable from having broken the thing yourself.
- **The dev host is a laptop that sleeps mid-session.** An agent stall or a wild wall-clock outlier usually means host suspend, not a hang or a regression — resume or re-run; never record the outlier as a finding. Bimodal timing data (tight cluster + a few enormous outliers) is the signature.
- **Subagents get an explicit model, never the session default.** Opus for anything with judgment in it — design, debugging, review/verification, commits touching kernel/scheduler/soundd semantics. Sonnet only for zero-decision mechanical work from an exact spec. Trivial edits: no agent, do them directly. When unsure, opus — one botched "mechanical" change costs more than the model delta saves.
- **Durable facts go in this file or `specs/`, not in private agent memory.** Owner's decision: everything lives in VCS where it is versioned and visible to every agent. Do not build up out-of-repo memory state.

## Diagnostics roadmap

Three layers, built in order; each is useful on its own.

1. **Process accounting (counters).** Cumulative per-process wall and user/kernel CPU time, page faults by cause, I/O ops and bytes, time blocked by reason. Incremented at existing kernel sites, read by one syscall, printed by a userland `stats` tool. **Built** — but the syscall reports only an *exited direct child*, exactly once, so it cannot sample a live daemon (known-issues §5).
2. **Event tracing.** Per-process ring of `(timestamp_ns, TraceEvent)`: syscall and fault entry/exit, I/O submit/complete, scheduled (with runqueue wait), preempted (with timeslice used), blocked/woken (with reason), lib load/relocate. ~24 bytes/event, 4096 entries (~96 KB), ~8 instrumented sites, one syscall to read. Answers "where is time going, in what order?"
3. **RIP sampling.** Per-process `(timestamp_ns, rip)` ring on timer tick. Needs frame-pointer unwinding to be worth anything — flat RIP profiles without call stacks are nearly worthless. Build only once layers 1–2 confirm something is CPU-bound.

## Planned architecture (specs written, implementation staged)

Read the spec before touching the subsystem it covers.

- `specs/scheduler-core-spec.md` — ownership-typed scheduler core as a `no_std` crate (`toyos-sched/`) with a deterministic host simulator and interleaving fuzzer; per-CPU exclusive queues, message-passing wakes, 10-stage always-green migration. **Stage 7c done: the kernel drives the core, with balance on, and the legacy notification path is deleted.** The driver half is `kernel/src/sched/`; `kernel/src/scheduler.rs` is the kernel-facing API and nothing else — the spec's "7c removes `scheduler.rs`" means the legacy body, which died at 7a (migration log records the divergence). Host tests: `cargo test` inside `toyos-sched/` (~15 s). Stage 4's exit criterion runs from the CLI: `cargo run --release -p toyos-sched-sim -- gate 10000` and `-- fuzz-sweep 10000000`. Five negative gates prove the harnesses have teeth — do not weaken one to make a change pass. Migration state, the gates, and every defect the cutover found: `specs/scheduler-migration-log.md`.
- `specs/capability-handles-spec.md` — refcounted kernel objects behind typed per-process handles (Fd→Handle); subsumes the SharedToken/io_uring/Fd debt in known issues.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one wait-free completion primitive, one park/recheck site.
- `specs/metal-boot-plan.md` — first boot on real hardware (ThinkPad T14 Gen 2), integrated keyboard + touchpad. Staged M0–M5; **M0, M1 and M2 built, and the flash-trigger condition is met** — `metal_sim_input` proves an injected keystroke and a relative pointer delta reach a userland process with the i8042 as the machine's only input device. The one thing QEMU cannot decide is whether the T14's EC lands in scancode set 2 with translation on; the driver's `0xF0 0x00` read-back determines the wire format and refuses to attach to one it did not ask for, so that is one line on the laptop's own screen rather than a bisect. Also the only honest instrument for the 2× performance bar.
- `specs/net-gate-plan.md` — gate N, the network analogue of gate A: pcap as device-side ground truth, slirp + harness-as-peer configs, adversarial frames and deterministic impairment. Scheduled after the first bare-metal attempt.
- `specs/device-test-strategy.md` — the general rule the gates are instances of: ground truth at the hardware boundary, the harness as actuator, and device *shape and lifecycle* tested before protocol depth (every driver defect so far came from changing which devices exist).
- `specs/metal-track-history.md` — what the metal-track review waves found: ~70 confirmed defects in code whose own suites were green, the twelve certifications that could not fail, and the seven review findings that were refuted. Read it before deciding a change is done because the tests pass — a green suite says the tests pass, not that the change is right; prove teeth in both directions, and read the code rather than the bug database.

## Known issues

One line each. **`specs/known-issues.md` has the detail and is the file to update.** Nothing stays here after it is fixed — the code and `git log` carry that.

- **One class, four instances: an id or a name treated as a capability, and a reference that outlives the object it names.** `be604ef` closed the pipe-id hole (`SYS_PIPE_OPEN` now needs creator/holder/socket-peer, with a real exploit test) and `e42532f` the stale-listener hijack (`Descriptor::Listener` holds a never-reused `ListenerId`, not a name) — but the first is a relationship check, not a capability, and its stated residual stands: **a peer entitled to one of a creator's pipes is entitled to all of them.** Still open: `SYS_LISTEN` (no namespace), `SYS_GRANT_SHARED` (re-grantable, unvalidated target, no revoke), `SYS_SET_KEYBOARD_LAYOUT`, `SharedToken` as a bare `u32`. And the outliving half is a **live cross-process data leak**: a `FileBacking` survives unlink, and on `/home` reads blocks the allocator has since given to another file. `specs/capability-handles-spec.md` is what makes both unrepresentable. Existing `device::is_owner` gates are only as strong as the claim — `SYS_OPEN_DEVICE` is first-come, so "gated" is not "privileged", including for `SYS_SET_RT_PRIORITY`.
- **Userland can still panic the kernel, now from the filesystem rather than from ELF.** A plain 3 MiB `fs::write` to `/home` underflows `bcachefs/src/btree.rs:184` via `split_node`. Also open: `SYS_SYSINFO` allocates one entry per live thread; `SYS_DLOPEN` never dedups and `SYS_DLCLOSE` is a no-op, so VA still grows without bound (the panic is closed; dedup is a semantics change, not a bounds check). `mm::align_2m` has no checked form and four callers size it from a device or userland. The crafted-ELF panic class is closed — and its lesson is the one to carry: a rebase with a wrapping add could place a demand-paged VMA in the kernel half, because `sys_mmap` had the range check and the loader reaching the same machinery did not. **Userland cannot create a loadable file above ~2 MiB at all** — nothing under `/tmp` is loadable (`vfs.rs:62` has no `open_backing`) and `/home` panics above that, so the ELF allocation-ceiling tests assert on declared length rather than reaching the heap assert. VA exhaustion is untestable too (the PMM refuses first).
- **Two xHCI first-boot risks remain**: no xECP/USBLEGSUP ownership handoff before the unconditional HCRST (QEMU cannot fail this; Lenovo firmware can), and USB hotplug does nothing at all — reachable now that M1 removed the zero-HID panic, and the record now says what it must take (an enumeration lock; the event demux is already by slot id). Also: a slot is never given back, on four paths.
- **No physical memory fairness.** No per-process limits, no pressure signal, no OOM killer; one process can starve the system. **Compounded by two userland programs that bound nothing they accept**: the compositor (`Poller::new(256)`, unguarded `windows.push`) and netd (`Poller::new(64)`) have no `MAX_` constant of any kind, so an unbounded window count is a memory-growth path any client can drive. Same class as `MAX_USER_STR` and `MAX_FDS`; nobody wrote these two.
- **A panic while holding `PROCESS_TABLE` hangs that CPU**; a panic while the virtio-console TX queue is wedged spins in `submit_and_wait`; a panic holding the allocator lock wedges the recovered CPU's next alloc or free — and that one needs no injection: `alloc.rs`'s >2 MiB assert fires *inside* the dlmalloc lock, so it and the missing bound are one defect (audit in progress). The on-screen console carries the captured *report* only, and **nothing distinguishes `panic_console::capture` from a no-op** — with its body replaced by `return`, `screen_late_panic` still passes. `crash_report`'s `try_lock` hazard is closed at `bd12795` (`preempt::enable` declines while `fault_state` is set), at the cost of a leaked `fault_state` now being a hang, and with no test that could tell the fix from a no-op.
- **The virtio-console has no line atomicity between writers.** Kernel `log!` and userspace `println!` interleave mid-word, corrupting machine-readable serial output; `tests/common/audio.rs` works around it reader-side. Whole-line writes should be atomic.
- **`sys_read` blocks on an empty Keyboard fd but returns `NotFound` on an empty Mouse fd.** Two fds of the same shape, two answers. And a sustained flood into a thread blocked there panics the kernel on `prepare_wait`'s "a task waits on at most one queue" — seen twice, not reduced.
- **Two scheduler leaks/looseness:** a thread retired while parked leaks its wait-queue node (the intrusive `wait_node` the core still owes fixes it); `handle_retire`'s `need_resched` on a running target is a request the next pass may decline (conformant — §7.6 bounds it by the quantum). **The fair split degrades as the machine widens** — worst service spread 30 ms at 1 CPU vs 1056 ms at 24 in the simulator, ~35x; simulator-only so far, and whether it is the policy or the model is not yet established. Found only because I5 measures *service* rather than checking vruntime against itself. `src/build.rs` cannot enable `sched-check`, so no CI run exercises the check build.
- **A syscall runs with interrupts masked** (`MSR_FMASK` clears IF, nothing on the path re-enables) and the entry level keeps the preempt count above zero, so `preempt::enable`'s slow path is not the RT-wake safe point scheduler spec §7.4 counts on. Masking is the stronger blocker: a remote resched is only deliverable as an IPI, which a masked CPU will not take. Also: `retire_task` is never reached by `cargo test`.
- **soundd: two audit defects open, one refuted, three fixed.** Open and assigned: hardcoded cpal format (needs the quiet-tree window — the fork edit redirects cpal for every agent), per-client shm never freed (merged with `SYS_GRANT_SHARED` revocation; revoke and reclaim are one mechanism). Fixed at `4fce59c`: PCM caps now select the format (**unverified by a QEMU boot** — seven attempts died in toolchain contention; `cargo test -- audio` is owed), passthrough gain (32,703 of 65,536 i16 values did not survive a round trip; now 0, exhaustively gated), `AudioInfo::as_bytes` padding. **Refuted, kept for the measurement**: the correlated TPDF dither draws — χ²/df ≈ 1.00 on the summand pair, indistinguishable from independent streams. **Three items are one, blocked on the cpal fork**: killing wedged clients, suspending on no progress, and resume all need the same missing client→soundd resume message — `MixCommand` has exactly three variants and none is resume, and there is no client→soundd traffic in the steady state at all. Consequence today: one paused client defeats idle suspend for the process's life (`is_streaming` latches and never clears — a per-period wake forever, battery-relevant on the T14). A soundd-only variant, stopping the device voice while keeping the timer wake, is blocked on the audio gate instead and could land first. Separately, a device advertising four buffers panics soundd at startup (`num_buffers > 5`), because `slot_count = num_buffers` assumed an equality the reasoning only requires as `>=`. Audit item (1) is re-filed: never an SQ overrun (the submission ring self-limits at four points) but **silent CQ completion loss** — `post_cqe` increments `dropped` and returns, and `Poller::wait` never reads it, so a caller blocks forever on a discarded event. Its doc comment still claims the kernel "asserts rather than overflows".
- **Gate A can still fail a run on `drains` alone**, with no gap and no underrun. A per-run failure should require evidence of harm.
- **A keyboard that resets behind our back is undetectable on the PS/2 wire** — under controller translation `0xAA` is left Shift's break code. Survivable, untested on metal.
- **Diagnostics:** on an idle machine the log ring flushes one line behind, so the last line before a quiet period is not evidence of anything, and on the shutdown path nothing drains at all before `acpi::shutdown()` — a shutdown that dies mid-sync is silent, including on metal; `dump_blocked` lost its cross-CPU view at Stage 7a and prints only the calling CPU's parked map (`ps`/`stats` were rebuilt on published counters and are accurate); the "~half the CPU is unattributed" claim was wrong and backwards, the gap being `ps` averaging a lifetime against a one-second delta (`specs/cpu-attribution.md`); profiling layers 2 and 3 (event tracing, RIP sampling) are not built.
- **std leaks the whole 2 MiB stack on every `thread::spawn`** — nothing records the pointer, so `join` cannot free it. One leaked kernel region per spawn; any per-process memory number across a threaded workload is wrong.
- **The fork estate is systematically outside every check the tree runs on itself** — invisible to the zero-warning bar (`--cap-lints allow`), invisible to ABI signature changes until a build breaks, able to hold frozen git-sourced copies of first-party crates that `[patch]` does not redirect, and fixable only in a quiet-tree window since editing a fork changes the build for every agent. Every fork problem so far surfaced as a surprise, never as a red test. Corollary with teeth: **"I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`** — grepping the monorepo is a partial enumeration that reads as complete. Framing and procedure: `specs/fork-lint-audit-plan.md`. Separately, the `memmap2` fork is 165 lines of unreachable code — delete `src/toyos.rs` or drop the toyos gate in `rustc_data_structures`, exactly one of the two.
- **Design debt:** io_uring abuses `shared_memory`; `SharedToken` is a bare `u32` with no RAII; `Fd` should be `Handle`; `KernelSlice::from_raw` trusts the caller's size — allocators should construct the slice; `gpu::set_resolution` frees the old framebuffer while consumers may hold pointers to it.
- **A machine with no serial has no output channel once boot is done** — the log ring drains into nothing and the screen only repaints at checkpoints and fatal panics, so the T14 is mute from `Boot: complete` until the compositor's terminal exists. Also: a keyboard or mouse claim succeeds on a machine with neither.
- **Hardware gaps:** PCID/INVPCID untested outside TCG; TLB shootdowns IPI every CPU for a full flush; the LAPIC timer is one-shot where TSC-deadline would be exact; every network client burns a second of `connect_blocking` retry on a machine with no NIC; the bootloader picks the most-pixels GOP mode, a square 2048x2048 on QEMU stdvga.
- Two unreproduced observations: `ps` appeared to stall for >2 s under heavy single-core load; and Doom's music was heard once at roughly half speed, never reproduced, with the wav capture measuring 1.00x (host contention suspected). If either recurs, capture before restarting — for audio that means **reading Doom's `[music]` RTF telemetry and soundd's wake/underrun stats, not listening**: a starved synthesizer and a wrong playback clock sound identical, and RTF is what separates them.
