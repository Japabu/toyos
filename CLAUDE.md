# ToyOS

A production-grade operating system built from scratch in Rust. Targets modern x86-64 hardware (2020+), UEFI only. ARM64 planned — keep architecture portable.

The name has no meaning. This is not a hobby project. The quality bar is the same as shipping software: correct, efficient, minimal, zero technical debt. The codebase is regularly scrutinized and refactored. Nothing is sacred except the principles below.

## Where the rest of this lives

**This file is what every agent needs before it knows which subsystem it is in.** Subsystem detail lives in a `CLAUDE.md` beside the code and loads only when you read a file in that subtree:

| | |
|---|---|
| `kernel/CLAUDE.md` | memory, processes, scheduling, filesystem, storage, input, USB, PCI, faults, the panic console |
| `userland/CLAUDE.md` | daemons, IPC, the compositor and display, surfaces, the POSIX layer |
| `tests/CLAUDE.md` | the harness, machine shapes, gate A, expected failures |
| `src/CLAUDE.md` | the build system, boot modes, the build and guest locks |

**That loading is lazy and it is not guaranteed.** Verified 2026-08-08: a subagent gets a subdirectory file when it `Read`s a file in that subtree, and does **not** get one from `Bash`. So a rule whose violation is unrecoverable or invisible stays here, however subsystem-flavoured it reads; a subdirectory file carries what you cannot use until you are already reading that subtree. When in doubt it goes here.

## Principles

- **Zero legacy.** No backwards compatibility. No fallbacks. No workarounds. No BIOS. No 32-bit. Research state-of-the-art OS design instead of replicating what older OSes do. We have no legacy to maintain — exploit that.
- **Zero technical debt.** Every feature is scrutinized. Dead code is deleted. Every abstraction earns its place.
- **Fail fast.** Panics over silent degradation. Exhaustive matches with panics for unexpected values. Never mask bugs. If something is unimplemented, the system screams and dies loudly. **It stops at the C boundary**: `extern "C"` has no unwind path, so a panic crossing one is `abort` and kills the process rather than the subsystem. Every Rust function C calls catches its own panics and answers as if absent (`userland/doom/src/ffi.rs`).
- **Corollary: fail-fast is for kernel bugs, not for untrusted input.** An `expect()` on a value that crossed the trust boundary is a userland-triggered kernel panic wearing fail-fast's clothes. Untrusted input that cannot be satisfied returns `SyscallError::{ResourceExhausted, InvalidArgument}` — never a panic, and never a silent truncation of the request to make it fit. The bounds that enforce it are named `MAX_*` and sit on the primitive rather than at each call site. All are policy, not physics — a bound's second question is what the caller sees when it is hit.
- **Kernel must never crash from userland.** A buggy userland process must not be able to bring down the kernel. But if the kernel itself has a bug, it must crash loudly so we can fix it and harden.
- **Simplicity.** Prefer the simpler solution unless the complex one brings >2x improvement. We can simplify aggressively because we have no legacy constraints. **Effort is never an argument.** How big a change is is information for sequencing, never a reason to decline it. Decide by writing the code both ways and keeping the one that reads better; deleting code wins on that basis alone, and refactoring to clean up needs no further justification. Corollary for agents: never offer the owner a smaller deliverable because they are blocked — build the right thing and say how long it takes. They will ask if they want it quick.
- **Rust is first class.** Not POSIX. Not C. The entire OS is Rust-native. C-isms tolerated only when the Rust alternative adds no safety or value. Prefer compile-time safety: unrepresentable > checked at runtime > covered by tests. **Caveat with teeth: this kernel does not unwind, so a `Drop` guard constrains only paths where the value is actually dropped — and "killed by another CPU" is not one.** An RAII fix aimed at a kill-path leak is decoration. Ask of any safety type: which paths does this bind, and is the failing one among them.
- **Code must not lie about itself.** A **sentinel** is a C-ism: C needs one for lack of a sum type, and niche optimization makes `Option<&T>`/`Option<NonZeroU8>` the size of the bare type, so there is no cost argument. Correct only where a wire or hardware format defines one — decode it at the boundary, never carry it inward. A **signature promising a check it never performs** (`-> Option` that never returns `None`) is worse than no check. A **doc comment is a claim to verify**, not documentation. The tell: a comment explaining what a magic value means is the type you should have written.
- **Development ergonomics above all.** The ability to iterate fast matters more than feature count. Tooling comes first.
- **Self-hosting.** The north star is building ToyOS from within ToyOS. No LLVM dependency. Cranelift as codegen backend, and `specs/cranelift-backend-assessment.md` prices it: not a next step.
- **Efficient.** Never hog resources without purpose. Free memory when not used. Minimize kernel overhead. The OS must be fast and responsive. General improvements only — never optimize for one specific app. **Hard bar: every component at most 2× the cost of current production OSes** (Linux, macOS, Windows). Note TCG cannot measure this — the distortion is non-uniform, so only real hardware or same-session A/B counts.
- **No slop comments.** Never add comments that restate what the code does. No "auto-closes on drop", "returns the value", "loop through items". Comments explain *why*, not *what*. If the code needs a *what* comment to be understood, rewrite the code. **If a comment reads like a commit message — evidence, measurements, what the code used to do — it belongs in the commit message.** The form that slips through is a *why* beside an assertion that already enforces it: the assertion is the statement, compiler-checked, so the comment is restatement with a weaker guarantee. The tell is a comment that locates code — "the assertion below" — which addresses the reader of a diff, not the file, and rots when the code moves. The second tell is a comment that argues for a decision by reciting the measurements that produced it.

## Architecture

> A snapshot, and deliberately shallow — always read the code, and read the subsystem's own `CLAUDE.md` before touching it.

**Kernel** — minimal, and new additions must be discussed and justified. Resource management, scheduling, process lifecycle, filesystem, device arbitration. 2 MB pages, demand paging, PIE binaries, full SMP. `kernel/CLAUDE.md`.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel and serves clients over IPC; crash one and the kernel is fine. soundd drives both sound cards from userland and is the first driver to live there. `userland/CLAUDE.md`.

**Syscall ABI** — `toyos-abi/`. Struct layouts, syscall numbers, constants, typed wrappers. Completely unstable; read the code. **Never add or change a syscall without discussion, and a number a deleted syscall used is retired, never reused.** `toyos/` builds on it with typed handles, IPC framing, ports, namespaces and `surface` — userland uses `toyos`, kernel uses `toyos-abi` only.

**Capabilities** — **a process holds exactly what its parent moved into it, and there is nothing it can name to get more.** No registry, no connect-by-name, no pid that is authority: a `RawHandle` is a slot in one process's own table, `/bin/init` builds every program's namespace and device claims out of `system.toml` before it spawns it, and a handle a process does not hold is a bug in that process — the kernel ends it with exit 139 rather than answering a word it can ignore. `specs/capability-endowment-spec.md`.

**CPU state** — **a CPU's control registers come from one declaration, applied by the BSP and by every AP and asserted on each of them** (`arch/control_regs.rs`); **no read-modify-write decides what either holds** — `init_cr0`'s and `pat::init`'s are transient no-fill windows ending at a value fixed before they opened, and the trampoline's three OR in only what long mode needs before the declaration is reachable at all. Both registers are written whole: `CR0`'s value is a constant, `CR4`'s is required-plus-what-CPUID-offers — a function every CPU evaluates and must agree on. Until 2026-08-08 an AP kept what `INIT` left it — **caching disabled, `WP` and `NE` clear on every core but cpu0, for the whole history of the tree** — so every multi-CPU measurement this project has taken was of a machine that no longer exists, and what that cost is bare metal's to say: `specs/issues/kernel/ap-control-registers-inherit-init.md` has `control-regs-bench`'s rows and the measurement still owed. Gates: `control_regs`, `control_regs_verdict`, and `control_regs_negative`, which boots the `no-ap-control-regs` actuator and holds the verdict against a real divergent AP.

**Input** — **the kernel delivers key *transitions* and never what one types, and a surface is what turns one into the other.** `RawKeyEvent` is a HID usage plus the modifier mask; translation, layouts, dead keys and escape sequences are `toyos-keymap::Translator` in userland, one per surface. Both halves are invisible when violated — a keymap in the kernel compiles, and a surface that rebuilds the modifier mask works until another surface has had the focus — so the boundary is here and the mechanics are in the two subsystem files. Design: `specs/input-architecture.md`.

**POSIX compatibility** — the kernel ABI and SDK are Rust-native and capability-shaped. POSIX, where it is needed to run ecosystem software, belongs in a **userspace** layer (`userland/libc` — ours, not a fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

## Dependencies

Only **Rust** and **QEMU** (for development). Everything else is bootstrapped from Rust. **That is the bar, and not yet a description of the tree.** The rules it is judged against: no binary outside Rust and QEMU (a macOS binary is a hard no, and "only for tests" does not soften it), only general and widely used crates (one that does *our* job we write ourselves — a driver crate is never ok), no Python, and a fork is the sanctioned form of every third-party source. Ask of anything new: **could this ever run inside ToyOS?**

Nothing in the tree checks any of it. `specs/dependency-audit-2026-08-08.md` is the inventory and `specs/dependency-purpose-2026-08-08.md` says what each direct crate is *for*. **The standing failures are declared rather than removed** — the owner's ruling, not the fix: Python via `rust/x`, `cc` for every host link, and four macOS FAT tools. **What ships is licensed honestly**: `NOTICE` names every committed third-party file with its hash, upstream and licence. An image carrying `DOOM1.WAD` may not be sold.

- **toyos-ld** (`toyos-ld/`) — custom linker, used for bootloader, kernel and all userland. **Its output is reproducible, and the container types are what say so**: one iterated into the output is a `BTreeMap`/`BTreeSet`, one asked only whether it holds a name stays a hash container. That line is the whole rule, and it covers the archive member pull-in worklist as much as the symbol table.
- **toyos-cc** (`toyos-cc/`) — minimal C compiler, not meant to grow; exists to bootstrap tinycc and compile doomgeneric. **A layout or linkage change it does not implement is refused by name** — attribute, `asm`, pragma. Dropping one silently is a miscompilation.
- **rust/** — fork of the Rust compiler and std with ToyOS platform support (submodule). Auto-bootstraps. Kept up to date with upstream.

## Ecosystem forks

Nothing is vendored. Every forked crate lives in its own repository as a `toyos` branch based on a pinned upstream commit, consumed via `[patch.crates-io]` git dependencies. **`forks.toml` is the manifest** — upstream, base commit, delta size, tier, PR status. Keep it accurate; it is how the estate stays honest.

- A fresh `git clone` + `cargo run` works with no setup: cargo fetches the forks.
- To *edit* a fork: clone it beside the monorepo and list it in `.cargo/config.toml` (gitignored — see `.cargo/config.toml.example`). Commit and push to the fork repo; the monorepo only pins the branch.
- `git log <base>..toyos` in a fork is exactly the ToyOS delta — and exactly what an upstream PR should contain.
- Forks depend on ToyOS crates by **version**, never by path: a path escaping the fork's own repo cannot resolve once cargo checks it out alone.
- All changes must be upstream-mergeable. ToyOS aims to be a first-class platform in every important Rust ecosystem crate, alongside unix and windows. Comments included: upstream's idiom and density govern, the ToyOS story goes in the commit message, and a delta is reread as the upstream reviewer will read it before it is committed.
- Use `#[cfg(target_os = "toyos")]` — never hijack existing cfg gates. Add ToyOS as a new platform alongside existing ones; don't modify cross-platform code.
- Publishing `toyos-abi`, `toyos` and `window` to crates.io is the one blocker for actual upstream PRs. Not needed for local builds.

## Std library rules

- **Add ToyOS as a new platform alongside unix/windows/wasi — never hijack existing cfg gates.** This is the rule that actually governs; the rest are its consequences.
- Prefer ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`, anything with `toyos` in the path.
- A cross-platform file may be touched **only to add a target arm to an existing platform-dispatch site** — never to change cross-platform semantics or API shape. Every file in the fork without `toyos` in its path is a dispatch arm and should stay one. `library/alloc` and `library/core` have **zero** delta and should keep it.
- Cherry-picking an already-**merged** upstream commit is early convergence, not divergence, and is allowed. Copying an unmerged PR is not — it voids the promise that the fork delta is what an upstream PR would contain.

## Build & test

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU. `--build-only` skips the launch.
- `cargo test` runs the integration tests (boots QEMU headless, runs the harness inside ToyOS). `-- --nocapture` for serial output, `-- <substring>` to filter, `-- --list` to list. **Fast tier only** — 31 tests over the owner's 10 s line are held back, every run names them, `-- --nightly` runs them.
- **Agents verify through `cargo test`, never by launching `cargo run`** — the run path opens a QEMU window on the owner's desktop by design; the harness passes `-display none`.
- **`cargo test` and `cargo run` produce large output.** Always run them in the background so the Bash tool does not silently truncate — `... [N characters truncated] ...` means data was lost. Read the output file afterward. **ToyOS is fast** — a full boot is under a second and incremental builds usually finish in seconds, so never assume slowness.
- **Every guest binary is built with `[profile.toyos]`** — opt-level 2, debug info, debug-assertions and overflow-checks **on**. There is no `--release` flag: it bundles four knobs and turns two of them off silently, and both crafted-ELF kernel panics were *found* by an overflow check. `build::assert_overflow_checked` refuses to write an image whose kernel does not reference `attempt to add with overflow`. `userland/libc` is the one exception and says so where it is.
- **A full `cargo test` builds two kernels: the shipping one and one carrying every actuator**, armed by `--kernel-param` (`kernel/src/actuator.rs`). An actuator is never a cargo feature, a third is refused, and **whatever measures boots the shipping one**.
- Boot modes, the boot parameter, the build lock and the host's slots: `src/CLAUDE.md`. The harness, machine shapes, gate A and expected failures: `tests/CLAUDE.md`.
- **Every guest this host boots is TCG, and TCG is one vendor's reading of the ISA.** QEMU's `syscall`/`sysret` and segment-load helpers implement Intel's wording, so for anything whose correctness depends on *which* vendor executes it, a green suite here is not evidence — a missing `STAR[63:48]` RPL killed every user thread on AMD while this machine stayed green. The `guest` shards in `.github/workflows/ci.yml` are the only gate that executes those instructions. **TCG also prices instructions unlike hardware, and an atomic read-modify-write is the one that bites**: one `fetch_add` per log line, uncontended, cost 350 ms of boot. A plain store under a lock the code already holds is free; reach for `fetch_add` on a hot path only when nothing else will do.
- Host tests outside the QEMU suite: `cargo test` inside `toyos-sched/`, `toyos-ps2/`, `toyos-gpt/`, `toyos-elf/`, `toyos-cc/`, `toyos-ld/`, `toyos-hda/`, `toyos-pci/`, `toyos-xhci/`, `toyos-desktop/`, `kernel-loom/`, `kernel-span/`, `toyos-fat32/`, `toyos-fat32-check/`, `toyos-abi/` and `toyos-manifest/`. `userland/sshd` has them too, but `userland/.cargo/config.toml` cross-compiles, so they need the triple: `cargo test --target "$(rustc -vV | sed -n 's/^host: //p')"`. **`kernel-loom` is the only thing that checks a memory ordering** — x86's TSO hides a missing acquire edge from every guest test. **`toyos-xhci` is where the states QEMU cannot stage live**, and its simulator has no way to express waiting, so a driver that could only be written by spinning could not be written in it. `cargo test --lib` in the root runs the build lock's own gates, the format check on the two volumes the image builder writes, and the documentation budgets below — no guest, seconds.
- **A red build may be the build system, not the code — re-run in isolation before believing any single red.** The `stage1-std/<target>/dist/deps` temp-dir error means a concurrent build, never a broken checkout; the tell is that the same command succeeds between attempts. Wait and re-run — **never repair or force-rebuild the toolchain**. A `[build-lock] waiting …` line naming a holder is that working, not a hang. A refusal that your worktree and the shared sysroot "disagree about toyos-abi/src" is correct, not a broken tree — the build it stops links your kernel against another checkout's struct layouts and no test catches that.
- **The dev host is a laptop that sleeps mid-session, and the suite says so.** A run whose wall clock jumped against the monotonic one reports `INVL` per test and **exits 2** — not a red: those verdicts were taken across a stopped host and establish nothing, so re-run rather than investigate. A wild outlier *not* reported that way is a real finding.

## Repository layout

```
src/              Build system (the root cargo project, package name: toyos-build)
kernel/           Kernel
kernel-loom/      Loom models of `kernel/src/sync.rs`, beside the kernel and not in it
bootloader/       UEFI bootloader
userland/         All userland programs
toyos-abi/        Kernel ABI (types, constants, syscall numbers, syscall wrappers)
toyos/            Userland SDK (typed handles, IPC, ports, namespaces, surface, shm, net)
toyos-manifest/   The one definition of `/etc/system.manifest`: the build system renders,
                  /bin/init parses, and a round-trip test is what makes them one format
toyos-keymap/     Layouts, dead-key composition, key translation, layout detection
toyos-fat32/      FAT32 driver, read + write; no format path by design
toyos-fat32-check/ FAT32 volume checker from Microsoft's fatgen103, derived from neither
                  our writer nor our reader — the outside judge on every volume we write
toyos-elf/        ELF64 decoding (no_std, no alloc, forbid(unsafe_code))
toyos-gpt/        GPT parser (no_std, no alloc, forbid(unsafe_code))
toyos-hda/        HDA codec decoding and output-path selection, pure
toyos-pci/        MSI and MSI-X capability decoding, pure
toyos-desktop/    Every decision the compositor makes, pure
toyos-ld/         Custom linker
toyos-cc/         Custom C compiler
rust/             Rust compiler/std fork (submodule)
tests/            Integration tests (QEMU-based)
specs/            Specifications and investigation write-ups
specs/issues/     Known issues, one file per issue — see its README
system.toml       What to build and boot (diag/ and console/ carry the other two modes)
```

## Debugging

**A backtrace is named from the binary's own file** — `.symtab` and `.strtab` are read off whatever backs the executable, so a program run from a disk gets the same report as one from the initrd. **There is no DWARF anywhere** (`toyos-ld` drops every debug section), so a frame can carry a name and never a line number.

**LLDB via QEMU** — all binaries are PIE, addresses change every boot. Parse serial for `Kernel memory located at: 0x...` and load symbols with `--slide`; userland pid and base address are logged at `spawn:`. Use `breakpoint set -r <pattern>` for Rust symbols (`-n` does not work with `::` paths). `cargo run` in the background, then `gdb-remote 1234`. `--debug` pauses the kernel before init via the `DEBUG_WAIT` AtomicBool, enables QEMU's `-d int,cpu_reset` log at `/tmp/toyos-qemu-debug.log`, and parks QEMU on triple fault so the faulting CPU state stays inspectable.

**QMP** — socket at `/tmp/toyos-qmp.sock`, script at `.claude/qmp.py`: `"ls /bin"` types a string and Enter, `--raw ret` a single key, `--raw n --ctrl` a chord, `--screenshot <path>` captures the screen.

**Reading a frozen guest, without `cargo run`.** Any harness test with `BootOptions { qmp: true }` leaves a socket under `$TMPDIR/toyos-tests-<pid>/lane-<n>/`, so a background loop can interrogate a hung boot while the test sits in its own wait. `human-monitor-command` with `info registers -a` gives every vCPU's `RIP`, `RFL` and `HLT`, which is how a halted-awaiting-interrupt machine is told from a wedged one. **Take that capture before injecting anything** — a keystroke revives a halted CPU, so Ctrl+Alt+D over the same socket both confirms the diagnosis and destroys the evidence for it.

**Ctrl+Alt+D is the blocked-task dump** and **Ctrl+Alt+F1 the panic console's pager** — what to press on a machine that stopped without panicking, and what to ask the owner for. Both are `kernel/CLAUDE.md`.

**Audio verification**: `cargo run -- --smp N --dump-audio` captures device output to `/tmp/toyos-audio.wav` (parse to EOF — RIFF sizes stay 0 unless the guest shuts down cleanly). `cargo test -- audio` runs the glitch regression tests. soundd prints wake/underrun/latency stats every ~2 s while clients exist; doom prints `[music]` telemetry every ~5 s.

## Workflow

**One agent, one worktree, one branch.** `cargo run -- --worktree add <path>` makes one in about a second and 23 MiB; `specs/worktrees.md` is the model and the protocol. Never `git worktree add` by hand — the naive path clones 913 MiB of rust history and starts a second toolchain that takes the machine-global `toyos` rustup name away from every other checkout. **The primary checkout is not a workspace**: it owns `rust/` (50 GiB), the rustup link and `main`, and `cargo run -- --sync` moves its tree onto whatever GitHub merged.

- Stay focused on the current task. **File what you find in `specs/issues/` and do not go fix it** — a separate agent will. One file per issue; its README has the shape.
- If something is blocking, stop and report it. Don't work around it.
- Never degrade audible or visual quality — even temporarily, even for a big win elsewhere — without the owner's explicit sign-off. Quality tradeoffs are the owner's call.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce output. If a command produces a lot of output or takes long, run it in the background.
- **Always be empirical.** Never assume a command succeeded or failed — read the actual output. Never assume code works — run it. Never guess at root causes — investigate.
- **Any number you write down — commit message, spec, or the rare comment that earns one — must come from a command that was actually run.** If a figure is an estimate, or a bound from a datasheet, it says so. A plausible invented number is worse than no number: the next reader has no reason to doubt it, and it survives every review that only checks whether the prose reads sensibly. **Write the message with `git commit -F <file>`, never `-m` with a backtick in it** — a double-quoted `-m` substitutes backticks, so the message silently loses that text *and the shell runs it*.
- **Commit freely on your branch; land through a pull request.** In your own worktree the tree, the index and the branch are yours alone: `git add -A`, `git stash` and an intermediate commit that does not compile are all fine. **`main` moves only through a merged pull request, and `cargo run -- --pr` is the whole local half** — the preflight refusals, `git fetch`, this host's `main` fast-forwarded, `git merge --no-ff origin/main` into your branch, `git push -u`. Then `gh pr create --draft` **at the first push** — CI runs on a pull request and on nothing else, so a branch without one is ungated for however long it lives — `gh pr ready` and a written `--title`/`--body-file` when it is finished (never `--fill`), `gh pr merge --auto --merge`, and `cargo run -- --sync` when it has merged. `--land` is retired; never merge into `main` by hand. **Your branch must contain `origin/main` or the merge button stays shut** — that is what makes CI's verdict a verdict on the *merged result*. **The pull request's title and body become the merge commit's**, so write them as main's record; `git log --no-merges` reads the work rather than the landings. **An ABI change lands on its own pull request first**: the sysroot claim refuses every other worktree for as long as you hold it, and both `--pr` and CI's `abi-split` check refuse a branch that mixes the two — an `Abi-Inseparable: <why>` commit trailer declares a split that genuinely cannot be made, out loud. **Every merge must leave `main`'s tip compiling** even though your branch's intermediate commits need not, and callee-before-caller still binds. **The ecosystem forks are the exception**: a fork clone in `.cargo/config.toml` is one directory every worktree shares, so explicit paths, no `stash`, no branch switching.
- **Never rewrite history, and never touch `main`.** No `git commit --amend`, no `git rebase`, no `--force`, on your own branch as much as anywhere: a branch is merged by hash and a pushed one is a hash somebody's CI run may already have cited. Squash and rebase merges are off at GitHub for the same reason. Push your own branch as often as you like. **`main` is protected** — pull request required, force-push and deletion refused, no bypass.
- **CI and this host are two instruments, and a red on one is not a red on the other until you name what differs** (`specs/ci-plan.md` §8). CI is KVM on four native x86-64 cores, **one guest per machine**, so it is the only gate on anything vendor-specific or speed-dependent — and it can say *nothing* about contention, which is the dev host's whole `ALONE: GREEN` class. **A red is known only if `cargo run -- --known-red <test>` says so** (`src/redlist.rs`). Do not read a CI green as coverage of a contention bug. **`guest-suite` is required**: its red blocks the merge.
- **Host load is not an excuse — the owner's ruling (2026-08-04).** A load-coincident audio failure is investigated as a real defect of the pipeline, never re-run away as noise; the owner accepts the risk this assumption is wrong, and evidence against it goes to him rather than into quiet workarounds. No measurement locks, no quiet-host scheduling. What still holds: A/B in the same session against the same HEAD, and gate A's *fast* tier verdict is harm-based. **Every gate A run records the host it was taken on and adjudicates nothing on it.**
- **Wait in the FOREGROUND; do not tight-poll a background job.** Background-task notifications work reliably for the *main* agent, but spawned subagents do not stop cleanly when they background work and wait — observed repeatedly, cause unknown. Run long commands in the foreground with an explicit `timeout` (default is only 120000 ms; the max is 600000). For work exceeding 10 minutes, background it once and block with two or three long foreground waits on a marker file — never hundreds of short polls. This is the opposite of what the main agent's tooling advises; subagents differ. Always poll before sleeping.
- **Subagents get an explicit model, never the session default.** Opus for anything with judgment in it — design, debugging, review/verification, commits touching kernel/scheduler/soundd semantics. Sonnet only for zero-decision mechanical work from an exact spec. Trivial edits: no agent, do them directly. When unsure, opus.
- **Durable facts go in this file, a subdirectory `CLAUDE.md`, or `specs/` — not in private agent memory.** Owner's decision: everything lives in VCS where it is versioned and visible to every agent.

### Keeping the documentation honest

**Root `CLAUDE.md` is budgeted in bytes, not lines** — a line-count limit cannot see growth when one line can be an essay, and one here reached 3,220 characters. `cargo test --lib` asserts the budget in `src/docs.rs`; it fails the build, so the bar binds rather than being aspirational.

**The rule ratchets both ways.** Adding to any `CLAUDE.md` means naming what it replaces or where it goes instead — every agent can argue a future one must not miss its finding, and none is ever asked to remove, which is how 240 lines became 13,000 words in a fortnight. Detail belongs in `specs/`, subsystem detail in the subsystem's own file, and **resolved narrative in `git log`** — the slop-comment principle applies to this file at scale, and most of what it has lost was evidence for a decision rather than the decision.

After each task, audit the file that owns what you changed and update it. If your change is only a story about how something was found, it belongs in the commit message and nowhere else.

## Planned architecture

Read the spec before touching the subsystem it covers.

- `specs/scheduler-core-spec.md` — the ownership-typed scheduler core (`toyos-sched/`), with a deterministic host simulator and interleaving fuzzer. **Stage 7c done.** Five negative gates prove the harnesses have teeth — do not weaken one to make a change pass. `specs/scheduler-migration-log.md` has the state and every defect the cutover found.
- `specs/iommu-spec.md` + `specs/userspace-drivers-spec.md` — `kernel/src/iommu/`, and the userspace drivers it exists to make safe. **Stages I0–I2 done**: every profile boots with its unit translating, every enumerated function in one identity-mapped domain, a DMA fault names the device and address before halting. Nothing is refused yet.
- `specs/hda-driver-plan.md` — HDA on the T14, the userspace-driver initiative's first device, stages H0–H10. **The owner decided the shape on 2026-08-06 and the line is who writes an address**: soundd reaches every write through a kernel call taking a buffer handle and never a physical address, over an allow-list refused by name. **H0, H1, H2 and H4 are built and a tone comes out of an `intel-hda` in QEMU; H3 is cleanup rather than a prerequisite.** That takes the IOMMU off audio's critical path. Two shape changes are in §4.1.6 for the owner to accept or reject.
- `specs/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one wait-free completion primitive, one park/recheck site.
- `specs/metal-boot-plan.md` — first boot on the ThinkPad T14 Gen 2: achieved. `specs/metal-hardware-inventory.md` records the machine.
- `specs/net-gate-plan.md` — gate N, the network analogue of gate A. Its metal arm waits on a NIC.
- `specs/wlan-plan.md` — WLAN on the T14's AX210, all scope questions settled, stages W0–W10. Waits behind the doom milestone.
- `specs/introspection-plan.md` — the read surface (`SYS_QUERY`/`SYS_LOG_READ`), daemon status protocol, toybox diag tools, disk adopt-by-witness. W7 is gated on the owner's review of its §4.
- `specs/blocking-io-plan.md` — "a CPU never waits for a device", stages B1–B5, and the measurement that says the xHCI driver is not where it is lost. Upstream of the spec above: its waits belong to a thread that asked to wait, these belong to one that asked to write a file.
- `specs/device-test-strategy.md` — ground truth at the hardware boundary, the harness as actuator, device *shape and lifecycle* before protocol depth.
- `specs/metal-track-history.md` — ~70 defects confirmed in code whose own suites were green. **Teeth are necessary and not sufficient: mutating your implementation tests the paths you wrote, never the states you did not think to construct.**
- `specs/type-safety-audit/` — where each area sits on the unrepresentable > checked > tested ladder.

### Diagnostics roadmap

Three layers, built in order. **1. Process accounting** — cumulative per-process wall and CPU time, page faults by cause, I/O ops and bytes, time blocked by reason. Built, but the syscall reports only an *exited direct child*, exactly once. **2. Event tracing** — per-process ring of `(timestamp_ns, TraceEvent)`, ~24 bytes/event, one syscall to read; answers "where is time going, in what order?". **3. RIP sampling** — needs frame-pointer unwinding to be worth anything; build only once 1–2 confirm something is CPU-bound.

## Known issues

**`specs/issues/` is the list and the place to file** — one file per issue, `<area>/<slug>.md`, frontmatter saying `status`, `kind` and `opened`. Read the area before touching the subsystem it covers; its README says how to query it and how to close one. Nothing is duplicated here.

These bite an agent who is *not* working on the subsystem:

- **A known red is *declared*, never skipped.** `EXPECTED_FAILURES` (`tests/toyos.rs`) names the test, the task, the write-up and the failure messages the exemption covers — anything else that test says still reds. Such a run exits 0 and the gate takes it; it is not a *clean* run and its last line says so. **An entry must be able to fail the build by itself**, so it declares what makes it stale. Two stand: `desktop_window_child` pending #156 and `hda_tone`'s phase check pending #88. Do not reclassify or delete either.
- **A landing gate red on a test that is green alone is *not* therefore the host, and reading it that way cost a week.** The class is a verdict that expires on the host's clock: it does not merely fail on a busy host, it **cannot report anything else**, so every defect underneath it arrives wearing the same sentence. **So a wait on the guest is bounded by the guest** (`await_guest`, silence or a wedge, never a span of seconds), a retype loop is a count of attempts, and an injection is paced. What is left that a duration decides carries `STALLED:` and prints as `STALL` — still red, and named apart so nobody bisects it. `ALONE: red again` still means nothing on a loaded host and needs an A/B against `main` in the same session. **None of this is a licence to re-run an audio harm verdict away.**
- **The converse also holds**: a machine-wide kernel panic reds whichever test happened to be running, so that red's name is the workload and never the cause — including when the harness re-runs it alone and it reds again.
- **Gate A's fast tier reds intermittently, on `main` as much as on your branch.** Before believing it is yours, stash and re-run: `specs/issues/audio/audio-tone-load-fast-tier-intermittent.md`.
- **A boot that wedges before the idle loop produces no serial output at all** — the log ring's only drains are the timer tick and the idle loop, and neither runs during the boot phases. It looks exactly like a kernel that never started. **The corollary a freeze investigation keeps getting wrong: the end of a metal log is where the machine went quiet, never where it stopped.**
- **The idle loop runs housekeeping before `pass()`, so a woken CPU is late by whatever that costs — and on the T14 that was every audible pop.** Anything added to the idle loop is an audio change, and userland `println!` shares that ring. **And on a machine with nothing to run the idle loop does not run at all**, so a *diagnostic* placed there reports nothing exactly when it is needed; `diag-tick` caps the sleep and no shipping kernel can carry it.
- **`Command::output()` returns an empty stderr, always** — the toyos `output` asks `spawn` for the pipe and then drops it. A guest test asserting on a child's refusal message passes vacuously. Use `spawn()` + `wait_with_output()`.
- **No disk wait in this kernel can be made to park, and the driver is not why.** At the moment a transfer is waited for the CPU is **four ticket spinlocks deep** — `log_file::SINK`, `vfs::VFS`, `fat32_adapter::VOLUMES`, `xhci::XHCI` — each disabling preemption for its whole life; `io-depth-probe` measures it (4 from the idle loop, 5 from a syscall) and no reading of the call graph gives that number. Every userland write-back goes there too, so it is not the log sink: this kernel cannot touch a disk without pinning a CPU for the device round trip, and *that* is the T14's audio pops. `usb-slow-device` stages it on this host — `cargo test --test toyos-build -- audio_tone --slow-usb` takes soundd's worst wake from 7,117 to 165,948 µs. **`specs/blocking-io-plan.md` is the wave** and `specs/issues/audio/disk-wait-pins-a-cpu.md` the entry.
- **The fork estate is outside every check the tree runs on itself.** "I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`.
- **A filtered C-test run can be red for a daemon's line rather than for its own output.** `cargo test -- <name>` opens one capture window, soundd's boot lines land in it, and that family compares whole stdout. Judge it from a full run.
