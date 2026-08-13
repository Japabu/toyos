# ToyOS

A production-grade operating system built from scratch in Rust. Modern x86-64 hardware (2020+), UEFI only; ARM64 planned — keep the architecture portable. The quality bar is shipping software: correct, efficient, minimal, zero technical debt.

## Where the rest of this lives

**This file is what every agent needs before it knows which subsystem it is in.** Detail lives below it and loads when you go there:

| | |
|---|---|
| `kernel/CLAUDE.md` | memory, processes, scheduling, filesystem, storage, input, USB, PCI, faults, the panic console |
| `userland/CLAUDE.md` | daemons, IPC, the compositor and display, surfaces, the POSIX layer |
| `tests/CLAUDE.md` | the harness, machine shapes, gate A, expected failures |
| `src/CLAUDE.md` | the build system, boot modes, the build and guest locks |
| `specs/testing-strategy.md` | the testing law: instruments, tiers, the PR gate, the nightly |
| `specs/forks.md` | the ecosystem fork estate and the std library rules |
| `specs/debugging.md` | LLDB, QMP, frozen guests, audio verification |
| `specs/README.md` | the specs taxonomy: law, plans, assessments, reference, issues |

A subdirectory `CLAUDE.md` loads when a file in that subtree is `Read`, and not from `Bash`. A rule whose violation is unrecoverable or invisible stays here; everything else lives where the work is.

## Principles

- **Zero legacy.** No backwards compatibility, no fallbacks, no workarounds, no BIOS, no 32-bit. Research state-of-the-art OS design instead of replicating older OSes.
- **Zero technical debt.** Dead code is deleted. Every abstraction earns its place.
- **Fail fast, trust nothing.** Panics over silent degradation; exhaustive matches; the unimplemented dies loudly. Input that crossed a trust boundary is never trusted and never panics the kernel — it is refused.
- **The kernel never crashes from userland.** A kernel bug crashes loudly; a userland bug never reaches it.
- **Rust is first class.** Not POSIX, not C. Unrepresentable is best: prefer compile-time safety over runtime checks over tests.
- **Development ergonomics above all.** Iteration speed beats feature count; tooling comes first.

## Architecture

> A snapshot, deliberately shallow — always read the code.

**Kernel** — minimal; new additions are discussed and justified. Resource management, scheduling, process lifecycle, filesystem, device arbitration. 2 MB pages, demand paging, PIE binaries, full SMP.

**Userspace daemons** — compositor, netd, soundd, sshd. Each claims a device from the kernel and serves clients over IPC; crash one and the kernel is fine.

**Syscall ABI** — `toyos-abi/`: struct layouts, syscall numbers, typed wrappers; completely unstable, read the code. Never add or change a syscall without discussion; a deleted syscall's number is retired, never reused. `toyos/` builds on it with typed handles, IPC framing, ports, namespaces and `surface` — userland uses `toyos`, the kernel uses `toyos-abi` only.

**Capabilities** — a process holds exactly what its parent moved into it, and there is nothing it can name to get more. No registry, no connect-by-name, no pid-as-authority: `/bin/init` builds every program's namespace and device claims from `system.toml` before spawning it, and a handle a process does not hold is a bug in that process — the kernel ends it rather than answering a word it can ignore.

**CPU state** — a CPU's control registers come from one declaration, applied by the BSP and by every AP and asserted on each; no read-modify-write decides what either holds.

**Input** — the kernel delivers key *transitions*, never what one types; a surface turns one into the other. Translation, layouts, dead keys and escape sequences live in userland, one translator per surface.

**POSIX** — the kernel ABI and SDK are Rust-native and capability-shaped. POSIX lives in `userland/libc` (ours, not a fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

## Dependencies

Only **Rust** and **QEMU** (for development). The rules: no binary outside those two — a macOS binary is a hard no, and "only for tests" does not soften it; only general and widely used crates — one that does *our* job we write ourselves, and a driver crate never; no Python; a fork is the sanctioned form of every third-party source. Ask of anything new: could this ever run inside ToyOS?

The bar is not yet the tree. The standing failures are declared rather than removed — Python via `rust/x`, `cc` for every host link, four macOS FAT tools. `NOTICE` names every committed third-party file with its hash, upstream and licence; an image carrying `DOOM1.WAD` may not be sold.

- **toyos-ld** — custom linker for bootloader, kernel and all userland. Its output is reproducible, and the container types say so: anything iterated into the output is a `BTreeMap`/`BTreeSet`; a container asked only for membership stays hashed.
- **toyos-cc** — minimal C compiler; exists to bootstrap tinycc and compile doomgeneric, not to grow. A layout or linkage construct it does not implement is refused by name — dropping one silently is a miscompilation.
- **rust/** — Rust compiler/std fork with ToyOS platform support (submodule). Auto-bootstraps; kept current with upstream. Its rules: `specs/forks.md`.

## Build & test

`specs/testing-strategy.md` is the testing law. Operationally:

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU; `--build-only` skips the launch. `cargo test` runs the QEMU harness.
- **Agents verify through `cargo test`, never `cargo run`** — the run path opens a QEMU window on the owner's desktop by design; the harness runs headless.
- **Both produce large output**: run them in the background and read the output file — `[N characters truncated]` means data was lost. ToyOS is fast: a full boot is under a second, incremental builds finish in seconds.

## Repository layout

```
src/               Build system (the root cargo project, package name: toyos-build)
kernel/            Kernel
kernel-loom/       Loom models of `kernel/src/sync.rs`, beside the kernel and not in it
bootloader/        UEFI bootloader
userland/          All userland programs
toyos-abi/         Kernel ABI (types, constants, syscall numbers, syscall wrappers)
toyos/             Userland SDK (typed handles, IPC, ports, namespaces, surface, shm, net)
toyos-manifest/    The one definition of `/etc/system.manifest`
toyos-keymap/      Layouts, dead-key composition, key translation, layout detection
toyos-fat32/       FAT32 driver, read + write; no format path by design
toyos-fat32-check/ FAT32 checker from Microsoft's fatgen103 — the outside judge
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
system.toml        What to build and boot
```

## Workflow

**One agent, one worktree, one branch.** `cargo run -- --worktree add <path>` makes one; never `git worktree add` by hand — the naive path clones the rust fork's history and takes the machine-global toolchain name from every other checkout. The primary checkout is not a workspace: it owns `rust/`, the rustup link and `main`; `cargo run -- --sync` moves it onto whatever GitHub merged.

- Stay on the current task. File what you find in `specs/issues/` and do not go fix it; one file per issue, its README has the shape.
- If something blocks, stop and report it. Don't work around it.
- Never degrade audible or visual quality — even temporarily, even for a big win elsewhere — without the owner's explicit sign-off.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce it; long output runs in the background and is read from the file.
- **Always be empirical.** Read actual output; run the code; investigate root causes instead of guessing.
- **Every written number comes from a command that was run.** An estimate or datasheet bound says so. Write commit messages with `git commit -F <file>`, never `-m` — a double-quoted `-m` substitutes backticks and the shell runs them.
- **Commit freely on your branch; land through a pull request.** `main` moves only through a merged PR, and `cargo run -- --pr` is the whole local half. `gh pr create --draft` at the first push — CI runs on PRs and nothing else; `gh pr ready` plus a written `--title`/`--body-file` when finished (never `--fill`); `gh pr merge --auto --merge`; `cargo run -- --sync` after it lands. Never merge into `main` by hand. Your branch must contain `origin/main` or the merge button stays shut. The PR's title and body become the merge commit's: write them as main's record. An ABI change lands on its own PR first; `Abi-Inseparable: <why>` declares the split that genuinely cannot be made. Every merge leaves `main`'s tip compiling.
- **Never rewrite history, and never touch `main`.** No `--amend`, no `rebase`, no `--force` — on your own branch as much as anywhere: a pushed hash may already be cited. `main` is protected — PR required, no force-push, no deletion, no bypass.
- **A red is known only if `cargo run -- --known-red <test>` says so** (`src/redlist.rs`). A PR red not about the author's diff is adjudicated there and fixed at its owner, never re-run away.
- **Host load is not an excuse.** A load-coincident audio failure is investigated as a real defect, never re-run away as noise; evidence against that assumption goes to the owner, not into quiet workarounds.
- **Subagents wait in the foreground.** Background-task notifications do not reliably re-wake subagents: run long commands in the foreground with an explicit `timeout`, and for longer work background once and block with a few long foreground waits — never hundreds of polls. Always poll before sleeping.
- **Subagents get an explicit model, never the session default.** The orchestrator scopes, dispatches and verifies; it does not hand-work. Judgment-bearing coding gets Opus or stronger; mechanical execution from an exact brief gets Sonnet; non-coding mechanical work gets Haiku; trivial edits need no agent.
- **Durable facts go in this file, a subsystem `CLAUDE.md`, or `specs/` — never in private agent memory.** Detail belongs in `specs/`, subsystem detail in the subsystem's file, resolved narrative in `git log`. After each task, audit the file that owns what you changed.

## Planned work

- `specs/scheduler-core-spec.md` — the ownership-typed scheduler core; five negative gates prove the harnesses have teeth, never weaken one to make a change pass.
- `specs/iommu-spec.md` + `specs/plans/userspace-drivers-spec.md` — the IOMMU and the userspace drivers it makes safe.
- `specs/plans/hda-driver-plan.md` — HDA on the T14; the line is who writes an address: soundd never holds a physical address.
- `specs/plans/iouring-blocking-spec.md` + `specs/plans/blocking-io-plan.md` — io_uring as the only blocking mechanism; a CPU never waits for a device.
- `specs/plans/metal-boot-plan.md` — the T14 metal track; carries the metal session checklist.
- `specs/plans/net-gate-plan.md`, `specs/plans/wlan-plan.md`, `specs/plans/introspection-plan.md`, `specs/plans/diagnostics-roadmap.md` — queued tracks.
- `specs/device-test-strategy.md` — ground truth at the hardware boundary; device shape and lifecycle before protocol depth.
