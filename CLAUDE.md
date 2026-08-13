# ToyOS

A production-grade operating system built from scratch in Rust. Modern x86-64 hardware (2020+), UEFI only; ARM64 planned — keep the architecture portable. The quality bar is shipping software: correct, efficient, minimal, zero technical debt.

## Where the rest of this lives

**This file is what every agent needs before it knows which subsystem it is in.** Subsystem detail lives in a `CLAUDE.md` beside the code and loads only when you read a file in that subtree:

| | |
|---|---|
| `kernel/CLAUDE.md` | memory, processes, scheduling, filesystem, storage, input, USB, PCI, faults, the panic console |
| `userland/CLAUDE.md` | daemons, IPC, the compositor and display, surfaces, the POSIX layer |
| `tests/CLAUDE.md` | the harness, machine shapes, gate A, expected failures |
| `src/CLAUDE.md` | the build system, boot modes, the build and guest locks |

That loading is lazy and not guaranteed: a subagent gets a subdirectory file when it `Read`s a file in that subtree, and not from `Bash`. A rule whose violation is unrecoverable or invisible stays here; a subdirectory file carries what you cannot use until you are already reading that subtree.

## Principles

- **Zero legacy.** No backwards compatibility, no fallbacks, no workarounds, no BIOS, no 32-bit. Research state-of-the-art OS design instead of replicating older OSes.
- **Zero technical debt.** Dead code is deleted. Every abstraction earns its place.
- **Fail fast.** Panics over silent degradation; exhaustive matches with panics for unexpected values; the unimplemented dies loudly. It stops at the C boundary: `extern "C"` has no unwind path, so every Rust function C calls catches its own panics and answers as if absent (`userland/doom/src/ffi.rs`).
- **Fail-fast is for kernel bugs, not untrusted input.** An `expect()` on a value that crossed the trust boundary is a userland-triggered kernel panic. Untrusted input that cannot be satisfied returns `SyscallError::{ResourceExhausted, InvalidArgument}` — never a panic, never a silent truncation. The bounds are named `MAX_*` and sit on the primitive, not at call sites; a bound's second question is what the caller sees when it is hit.
- **The kernel never crashes from userland.** A kernel bug crashes loudly; a userland bug never reaches it.
- **Simplicity.** The simpler solution wins unless the complex one brings >2× improvement. Effort is never an argument: a change's size is sequencing information, never a reason to decline it. Decide by writing it both ways and keeping the one that reads better; deletion wins on that basis alone. Never offer a smaller deliverable because the right one is big.
- **Rust is first class.** Not POSIX, not C. C-isms only where the Rust alternative adds nothing. Prefer compile-time safety: unrepresentable > checked at runtime > covered by tests. This kernel does not unwind: a `Drop` guard binds only paths that actually drop the value, and "killed by another CPU" is not one — ask of any safety type which paths it binds and whether the failing one is among them.
- **Code must not lie about itself.** A sentinel is a C-ism; niche optimization makes `Option<&T>`/`Option<NonZeroU8>` the size of the bare type, so a sentinel is correct only where a wire or hardware format defines one — decoded at the boundary, never carried inward. A signature promising a check it never performs is worse than no check. A doc comment is a claim to verify. A comment explaining a magic value is the type you should have written.
- **Development ergonomics above all.** Iteration speed beats feature count; tooling comes first.
- **Self-hosting.** The north star is building ToyOS from within ToyOS: no LLVM, Cranelift as codegen backend — priced in `specs/assessments/cranelift-backend-assessment.md`, not a next step.
- **Efficient.** Free what is unused; minimize kernel overhead; general improvements only, never for one app. Hard bar: every component at most 2× the cost of current production OSes — measured on real hardware or same-session A/B, never under TCG.
- **No slop comments.** A comment explains *why*, never *what*; a *what* comment means the code needs rewriting. A comment that reads like a commit message — evidence, measurements, what the code used to do — belongs in the commit message. Two tells: a *why* beside an assertion that already enforces it, and a comment that locates code ("the assertion below") — both address the diff's reader and rot when the code moves.

## Architecture

> A snapshot, deliberately shallow — read the code, and the subsystem's `CLAUDE.md`, before touching anything.

**Kernel** — minimal; new additions are discussed and justified. Resource management, scheduling, process lifecycle, filesystem, device arbitration. 2 MB pages, demand paging, PIE binaries, full SMP.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel and serves clients over IPC; crash one and the kernel is fine.

**Syscall ABI** — `toyos-abi/`: struct layouts, syscall numbers, typed wrappers; completely unstable, read the code. Never add or change a syscall without discussion; a deleted syscall's number is retired, never reused. `toyos/` builds on it with typed handles, IPC framing, ports, namespaces and `surface` — userland uses `toyos`, the kernel uses `toyos-abi` only.

**Capabilities** — a process holds exactly what its parent moved into it, and there is nothing it can name to get more. No registry, no connect-by-name, no pid-as-authority: `/bin/init` builds every program's namespace and device claims from `system.toml` before spawning it, and a handle a process does not hold is a bug in that process — the kernel ends it with exit 139 rather than answering a word it can ignore. `specs/capability-endowment-spec.md`.

**CPU state** — a CPU's control registers come from one declaration, applied by the BSP and by every AP and asserted on each (`arch/control_regs.rs`); no read-modify-write decides what either holds. `CR0`'s value is a constant; `CR4`'s is required-plus-what-CPUID-offers, a function every CPU evaluates and must agree on. The consequence class only silicon can observe carries a metal-checklist entry (`specs/issues/kernel/ap-control-registers-inherit-init.md`). Gates: `control_regs`, `control_regs_verdict`, `control_regs_negative`.

**Input** — the kernel delivers key *transitions*, never what one types; a surface is what turns one into the other. `RawKeyEvent` is a HID usage plus the modifier mask; translation, layouts, dead keys and escape sequences are `toyos-keymap::Translator` in userland, one per surface. Both halves are invisible when violated — a keymap in the kernel compiles, and a surface that rebuilds the modifier mask works until another surface has had focus — so the boundary is stated here. `specs/input-architecture.md`.

**POSIX** — the kernel ABI and SDK are Rust-native and capability-shaped. POSIX lives in `userland/libc` (ours, not a fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

## Dependencies

Only **Rust** and **QEMU** (for development). The rules: no binary outside those two — a macOS binary is a hard no, and "only for tests" does not soften it; only general and widely used crates — one that does *our* job we write ourselves, and a driver crate never; no Python; a fork is the sanctioned form of every third-party source. Ask of anything new: could this ever run inside ToyOS?

The bar is not yet the tree. The standing failures are declared rather than removed — Python via `rust/x`, `cc` for every host link, four macOS FAT tools; `specs/assessments/dependency-audit-2026-08-08.md` is the inventory. `NOTICE` names every committed third-party file with its hash, upstream and licence; an image carrying `DOOM1.WAD` may not be sold.

- **toyos-ld** — custom linker for bootloader, kernel and all userland. Its output is reproducible, and the container types say so: anything iterated into the output is a `BTreeMap`/`BTreeSet`; a container asked only for membership stays hashed. That covers the archive pull-in worklist as much as the symbol table.
- **toyos-cc** — minimal C compiler; exists to bootstrap tinycc and compile doomgeneric, not to grow. A layout or linkage construct it does not implement is refused by name — dropping one silently is a miscompilation.
- **rust/** — Rust compiler/std fork with ToyOS platform support (submodule). Auto-bootstraps; kept current with upstream.

## Ecosystem forks

Nothing is vendored: every forked crate is its own repository's `toyos` branch on a pinned upstream commit, consumed via `[patch.crates-io]`. `forks.toml` is the manifest — upstream, base, delta, tier, PR status; keep it accurate.

- A fresh `git clone` + `cargo run` works with no setup.
- To edit a fork: clone it beside the monorepo and list it in `.cargo/config.toml` (gitignored). Commit and push to the fork repo; the monorepo pins the branch. Fork clones are shared by every worktree: explicit paths, no `stash`, no branch switching.
- `git log <base>..toyos` in a fork is exactly the ToyOS delta — and exactly the future upstream PR.
- Forks depend on ToyOS crates by version, never by path.
- Every change is upstream-mergeable: `#[cfg(target_os = "toyos")]` as a new platform beside existing ones, never modifying cross-platform code; upstream's comment idiom and density govern; the ToyOS story goes in the commit message.
- Publishing `toyos-abi`, `toyos` and `window` to crates.io is the one blocker for actual upstream PRs.

## Std library rules

- Add ToyOS as a new platform alongside unix/windows/wasi — never hijack existing cfg gates. The rest are consequences:
- Prefer ToyOS-specific files: `sys/pal/toyos/`, `os/toyos/`.
- A cross-platform file is touched only to add a target arm at an existing platform-dispatch site. `library/alloc` and `library/core` have zero delta and keep it.
- Cherry-picking a merged upstream commit is early convergence and allowed; copying an unmerged PR voids the promise that the fork delta is an upstream PR.

## Build & test

`specs/testing-strategy.md` is the testing law — instruments and their ownership, the tier rule, the pull-request gate, the nightly. This section is operational.

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU; `--build-only` skips the launch. `cargo test` runs the QEMU harness, Fast tier only; `-- --nightly` restores the withheld tier (`src/tiers.rs` is the declaration). `-- --nocapture` for serial, `-- <substring>` to filter, `-- --list` to list.
- **Agents verify through `cargo test`, never `cargo run`** — the run path opens a QEMU window on the owner's desktop by design; the harness passes `-display none`.
- **Both produce large output**: run them in the background and read the output file — `[N characters truncated]` means data was lost. ToyOS is fast: a full boot is under a second, incremental builds finish in seconds.
- Every guest binary is `[profile.toyos]` — opt-level 2, debug info, debug-assertions and overflow-checks on. There is no `--release`. `build::assert_overflow_checked` refuses an image whose kernel does not reference `attempt to add with overflow`; `userland/libc` is the declared exception.
- A full `cargo test` builds two kernels: the shipping one and one carrying every actuator, armed by `--kernel-param` (`kernel/src/actuator.rs`). An actuator is never a cargo feature; a third build is refused; whatever measures boots the shipping one.
- **Every guest this host boots is TCG — one vendor's reading of the ISA.** Anything vendor-dependent is gated only by CI's KVM shards: a missing `STAR[63:48]` RPL killed every user thread on AMD while this host stayed green. TCG also prices instructions unlike hardware — one uncontended `fetch_add` per log line cost 350 ms of boot; a plain store under a lock the code already holds is free.
- Host suites: `cargo test` inside `toyos-sched/`, `toyos-ps2/`, `toyos-gpt/`, `toyos-elf/`, `toyos-cc/`, `toyos-ld/`, `toyos-hda/`, `toyos-pci/`, `toyos-xhci/`, `toyos-desktop/`, `kernel-loom/`, `kernel-span/`, `toyos-fat32/`, `toyos-fat32-check/`, `toyos-abi/`, `toyos-manifest/`; `userland/sshd` cross-compiles and needs the host triple (`cargo test --target "$(rustc -vV | sed -n 's/^host: //p')"`). `kernel-loom` is the only memory-ordering check in the tree — x86 TSO hides a missing acquire edge from every guest test. `toyos-xhci`'s simulator cannot express waiting — a driver only writable by spinning cannot be written in it. Root `cargo test --lib` runs the build-lock gates, the image format checks and the documentation budgets — seconds, no guest.
- **A red build may be the build system — re-run in isolation before believing any single red.** A `stage1-std/<target>/dist/deps` temp-dir error means a concurrent build; a `[build-lock] waiting …` line naming a holder is working, not hung; never repair or force-rebuild the toolchain. A refusal that your worktree and the shared sysroot disagree about `toyos-abi/src` is correct — the build it stops links against another checkout's struct layouts and no test catches that.
- **The dev host is a laptop that sleeps mid-session, and the suite says so.** A run whose wall clock jumped against the monotonic one reports `INVL` per test and exits 2 — re-run rather than investigate. A wild outlier *not* marked that way is a real finding.
- Boot modes, the boot parameter, locks and host slots: `src/CLAUDE.md`. The harness, machine shapes, gate A and expected failures: `tests/CLAUDE.md`.

## Repository layout

```
src/               Build system (the root cargo project, package name: toyos-build)
kernel/            Kernel
kernel-loom/       Loom models of `kernel/src/sync.rs`, beside the kernel and not in it
bootloader/        UEFI bootloader
userland/          All userland programs
toyos-abi/         Kernel ABI (types, constants, syscall numbers, syscall wrappers)
toyos/             Userland SDK (typed handles, IPC, ports, namespaces, surface, shm, net)
toyos-manifest/    The one definition of `/etc/system.manifest`; a round-trip test keeps
                   the build system's writer and /bin/init's parser one format
toyos-keymap/      Layouts, dead-key composition, key translation, layout detection
toyos-fat32/       FAT32 driver, read + write; no format path by design
toyos-fat32-check/ FAT32 checker from Microsoft's fatgen103, derived from neither our
                   writer nor our reader — the outside judge on every volume we write
toyos-elf/         ELF64 decoding (no_std, no alloc, forbid(unsafe_code))
toyos-gpt/         GPT parser (no_std, no alloc, forbid(unsafe_code))
toyos-hda/         HDA codec decoding and output-path selection, pure
toyos-pci/         MSI and MSI-X capability decoding, pure
toyos-desktop/     Every decision the compositor makes, pure
toyos-ld/          Custom linker
toyos-cc/          Custom C compiler
rust/              Rust compiler/std fork (submodule)
tests/             Integration tests (QEMU-based)
specs/             Living normative documents (specs/README.md is the taxonomy)
specs/plans/       Staged intentions — a plan dies on completion
specs/assessments/ Dated evidence, frozen
specs/reference/   Non-normative fact sheets
specs/issues/      Known issues, one file per issue — see its README
system.toml        What to build and boot (diag/ and console/ carry the other two modes)
```

## Debugging

**A backtrace is named from the binary's own file** — `.symtab`/`.strtab` are read off whatever backs the executable. There is no DWARF debug info (`toyos-ld` drops every debug section): a frame carries a name, never a line number.

**LLDB via QEMU** — all binaries are PIE; addresses change every boot. Parse serial for `Kernel memory located at: 0x...` and load symbols with `--slide`; userland pid and base are logged at `spawn:`. Use `breakpoint set -r <pattern>` for Rust symbols (`-n` fails on `::` paths). `cargo run` in the background, then `gdb-remote 1234`. `--debug` pauses the kernel before init via `DEBUG_WAIT`, enables QEMU's `-d int,cpu_reset` log at `/tmp/toyos-qemu-debug.log`, and parks QEMU on triple fault.

**QMP** — socket at `/tmp/toyos-qmp.sock`, script at `.claude/qmp.py`: `"ls /bin"` types and enters, `--raw ret` one key, `--raw n --ctrl` a chord, `--screenshot <path>` captures.

**Reading a frozen guest without `cargo run`**: any harness test with `BootOptions { qmp: true }` leaves a socket under `$TMPDIR/toyos-tests-<pid>/lane-<n>/`. `human-monitor-command` with `info registers -a` gives every vCPU's `RIP`, `RFL` and `HLT` — how a halted machine is told from a wedged one. Take that capture before injecting anything: a keystroke revives a halted CPU, so Ctrl+Alt+D both confirms the diagnosis and destroys the evidence.

**Ctrl+Alt+D is the blocked-task dump, Ctrl+Alt+F1 the panic console's pager** — what to press on a machine that stopped without panicking. Both are `kernel/CLAUDE.md`.

**Audio**: `cargo run -- --smp N --dump-audio` captures device output to `/tmp/toyos-audio.wav` (parse to EOF — RIFF sizes stay 0 unless the guest shuts down cleanly). `cargo test -- audio` runs the glitch regressions. soundd prints wake/underrun/latency stats every ~2 s while clients exist; doom prints `[music]` telemetry every ~5 s.

## Workflow

**One agent, one worktree, one branch.** `cargo run -- --worktree add <path>` makes one in about a second and 23 MiB; `specs/worktrees.md` is the protocol. Never `git worktree add` by hand — the naive path clones the rust fork's history and takes the machine-global `toyos` rustup name from every other checkout. The primary checkout is not a workspace: it owns `rust/`, the rustup link and `main`; `cargo run -- --sync` moves it onto whatever GitHub merged.

- Stay on the current task. File what you find in `specs/issues/` and do not go fix it; one file per issue, its README has the shape.
- If something blocks, stop and report it. Don't work around it.
- Never degrade audible or visual quality — even temporarily, even for a big win elsewhere — without the owner's explicit sign-off.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce it; long output runs in the background and is read from the file.
- **Always be empirical.** Read actual output; run the code; investigate root causes instead of guessing.
- **Every written number comes from a command that was run.** An estimate or datasheet bound says so. A plausible invented number is worse than none: nothing prompts anyone to doubt it. Write commit messages with `git commit -F <file>`, never `-m` — a double-quoted `-m` substitutes backticks and the shell runs them.
- **Commit freely on your branch; land through a pull request.** Your worktree's tree, index and branch are yours: intermediate commits need not compile. `main` moves only through a merged PR, and `cargo run -- --pr` is the whole local half — preflight refusals, fetch, `main` fast-forwarded, `origin/main` merged in, push. `gh pr create --draft` at the first push — CI runs on PRs and nothing else; `gh pr ready` plus a written `--title`/`--body-file` when finished (never `--fill`); `gh pr merge --auto --merge`; `cargo run -- --sync` after it lands. Never merge into `main` by hand. Your branch must contain `origin/main` or the merge button stays shut — that is what makes CI's verdict about the merged result. The PR's title and body become the merge commit's: write them as main's record. An ABI change lands on its own PR first — the sysroot claim holds every other worktree while you have it; `--pr` and CI's `abi-split` refuse a mixed branch, and `Abi-Inseparable: <why>` declares the split that genuinely cannot be made. Every merge leaves `main`'s tip compiling.
- **Never rewrite history, and never touch `main`.** No `--amend`, no `rebase`, no `--force` — on your own branch as much as anywhere: a pushed hash may already be cited. Squash and rebase merges are off at GitHub for the same reason. `main` is protected — PR required, no force-push, no deletion, no bypass.
- **CI and this host are two instruments** — the law's §2 owns the map. A red on one is not a red on the other until you name what differs. **A red is known only if `cargo run -- --known-red <test>` says so** (`src/redlist.rs`); a PR red not about the author's diff is adjudicated there and fixed at its owner, never re-run away. `guest-suite` is required: its red blocks the merge.
- **Host load is not an excuse.** A load-coincident audio failure is investigated as a real defect, never re-run away as noise; evidence against that assumption goes to the owner, not into quiet workarounds. What holds: A/B in the same session against the same HEAD; gate A's fast-tier verdict is harm-based; every gate A run records its host and adjudicates nothing on it.
- **Subagents wait in the foreground.** Background-task notifications do not reliably re-wake subagents: run long commands in the foreground with an explicit `timeout` (max 600000 ms), and for longer work background once and block with two or three long foreground waits — never hundreds of polls. Always poll before sleeping. This is the opposite of the main agent's tooling advice; subagents differ.
- **Subagents get an explicit model, never the session default.** The orchestrator scopes, dispatches and verifies; it does not hand-work. Judgment-bearing coding — design, debugging, review, kernel/scheduler/soundd semantics — gets Opus or stronger. Mechanical execution from an exact brief gets Sonnet. Non-coding mechanical work gets Haiku. Trivial edits: no agent.
- **Durable facts go in this file, a subsystem `CLAUDE.md`, or `specs/` — never in private agent memory.** Everything lives in VCS, versioned and visible to every agent.

### Keeping the documentation honest

Root `CLAUDE.md` is budgeted in bytes — `cargo test --lib` asserts it in `src/docs.rs`, so the bar binds. The rule ratchets both ways: adding to any `CLAUDE.md` means naming what it replaces or where it goes instead. Detail belongs in `specs/`, subsystem detail in the subsystem's file, resolved narrative in `git log` — the slop-comment principle applied at file scale. After each task, audit the file that owns what you changed; a story about how something was found belongs in the commit message and nowhere else.

## Planned architecture

Read the spec before touching the subsystem it covers.

- `specs/scheduler-core-spec.md` — the ownership-typed scheduler core (`toyos-sched/`); its §11 has the state. Five negative gates prove the harnesses have teeth — never weaken one to make a change pass.
- `specs/iommu-spec.md` + `specs/plans/userspace-drivers-spec.md` — `kernel/src/iommu/` and the userspace drivers it makes safe. Stages I0–I2 done; nothing is refused yet.
- `specs/plans/hda-driver-plan.md` — HDA on the T14, the userspace-driver initiative's first device. The line is who writes an address: soundd reaches every write through a kernel call taking a buffer handle, never a physical address, over an allow-list refused by name. H0–H2 and H4 built; a tone comes out of QEMU's `intel-hda`.
- `specs/plans/iouring-blocking-spec.md` — io_uring as the only blocking mechanism; one wait-free completion primitive, one park/recheck site.
- `specs/plans/blocking-io-plan.md` — "a CPU never waits for a device", stages B1–B5. Upstream of the spec above.
- `specs/plans/metal-boot-plan.md` — the T14 metal track, first boot achieved; carries the metal session checklist. `specs/reference/metal-hardware-inventory.md` records the machine.
- `specs/plans/net-gate-plan.md` — gate N; its metal arm waits on a NIC.
- `specs/plans/wlan-plan.md` — WLAN on the T14's AX210, stages W0–W10, scope settled.
- `specs/plans/introspection-plan.md` — the read surface (`SYS_QUERY`/`SYS_LOG_READ`), daemon status, toybox diag tools; W7 gated on the owner's review of its §4.
- `specs/device-test-strategy.md` — ground truth at the hardware boundary; device shape and lifecycle before protocol depth.
- `specs/assessments/metal-track-history.md` — ~70 defects confirmed in code whose own suites were green. Teeth are necessary and not sufficient: mutating your implementation tests the paths you wrote, never the states you did not think to construct.
- `specs/assessments/type-safety-audit/` — where each area sits on the unrepresentable > checked > tested ladder.

### Diagnostics roadmap

Three layers, in order. **1. Process accounting** — per-process wall and CPU time, page faults by cause, I/O, time blocked by reason; built, but the syscall reports only an exited direct child, exactly once. **2. Event tracing** — per-process ring of `(timestamp_ns, TraceEvent)`, one syscall to read. **3. RIP sampling** — needs frame-pointer unwinding; build only once 1–2 confirm something is CPU-bound.

## Known issues

**`specs/issues/` is the list and the place to file** — one file per issue, `<area>/<slug>.md`, frontmatter with `status`, `kind`, `opened`. Read the area before touching its subsystem. Nothing is duplicated here. These bite an agent who is *not* working on the subsystem:

- **A known red is declared, never skipped.** `EXPECTED_FAILURES` (`tests/toyos.rs`) names the test, the task, the write-up and the exact failure messages exempted — anything else that test says still reds, and an entry declares what makes it stale. Two stand: `desktop_window_child` (#156) and `hda_tone`'s phase check (#88), both Nightly-tier, so the exemptions bind nightly runs. Do not reclassify or delete either.
- **A landing-gate red on a test that is green alone is not therefore the host.** The class is a verdict that expires on the host's clock: it cannot report anything else, so every defect underneath arrives wearing the same sentence. A wait on the guest is bounded by the guest, a retype loop is a count, an injection is paced; what a duration still decides prints `STALL` — red, and named apart so nobody bisects it. `ALONE: red again` on a loaded host means nothing without a same-session A/B against `main`. None of this re-runs an audio harm verdict away.
- **The converse holds too**: a machine-wide kernel panic reds whichever test was running — that red's name is the workload, never the cause, even when the isolated re-run reds again.
- **Gate A's fast tier reds intermittently, on `main` as much as your branch.** Stash and re-run before believing it is yours: `specs/issues/audio/audio-tone-load-fast-tier-intermittent.md`.
- **A boot that wedges before the idle loop produces no serial output at all** — the log ring's only drains are the timer tick and the idle loop, and neither runs during boot phases. The end of a metal log is where the machine went quiet, never where it stopped.
- **Anything added to the idle loop is an audio change** — housekeeping runs before `pass()`, so a woken CPU is late by what it costs, and userland `println!` shares that ring. On a machine with nothing to run the idle loop does not run at all, so a diagnostic placed there reports nothing exactly when needed.
- **No disk wait in this kernel can park, and the driver is not why.** At the moment a transfer is waited for, the CPU is four ticket spinlocks deep — `log_file::SINK`, `vfs::VFS`, `fat32_adapter::VOLUMES`, `xhci::XHCI` — each disabling preemption for its whole life (`io-depth-probe` measures 4 from the idle loop, 5 from a syscall). Every userland write-back goes there too: this kernel cannot touch a disk without pinning a CPU, and that is the T14's audio pops. `specs/plans/blocking-io-plan.md` is the wave; `specs/issues/audio/disk-wait-pins-a-cpu.md` the entry.
- **The fork estate is outside every check the tree runs on itself.** "I enumerated the call sites" is only true if the enumeration covered `~/.cargo/git/checkouts/`.
- **A filtered C-test run can be red for a daemon's line rather than its own output** — `cargo test -- <name>` opens one capture window, soundd's boot lines land in it, and that family compares whole stdout. Judge it from a full run.
