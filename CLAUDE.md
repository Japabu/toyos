# ToyOS

An operating system built from scratch in Rust, held to a production-grade engineering bar — the bar is the changes, not yet the product. Modern x86-64 hardware (2020+), UEFI only; ARM64 planned — keep the architecture portable. The quality bar is shipping software: correct, efficient, minimal, zero silent debt. A tracked weakness is still a weakness: the honest answer about current state is "known, tracked, still true" — never "we have an issue for that."

## Where the rest of this lives

**This file is what every agent needs before it knows which subsystem it is in.** Detail lives below it and loads when you go there:

| | |
|---|---|
| `kernel/CLAUDE.md` | the caveats that bite kernel work |
| `userland/CLAUDE.md` | the server doctrine, and the caveats that bite userland work |
| `tests/CLAUDE.md` | the caveats that bite the harness |
| `src/CLAUDE.md` | boot modes, the locks and slots, worktrees — the operational file |
| `issues/README.md` | the issue tracker: one file per issue, typed by kind; `ls` is the index |

There are no spec documents. Rules live where they are enforced — a gate, a
module header, the redlist — and everything else is an issue. Free text that
merely describes the tree rots and is deleted, not maintained.

A subdirectory `CLAUDE.md` loads when a file in that subtree is `Read`, and not from `Bash`. A rule whose violation is unrecoverable or invisible stays here; everything else lives where the work is.

## Principles

- **Zero legacy.** No backwards compatibility, no fallbacks, no workarounds, no BIOS, no 32-bit. Research state-of-the-art OS design instead of replicating older OSes.
- **Zero silent debt.** Dead code is deleted; every abstraction earns its place. A discovered compromise has exactly two legal outcomes: remove it, or record it with ownership, evidence and an exit condition — and it stays a present-state weakness until removed.
- **Fail fast, trust nothing.** Panics over silent degradation; exhaustive matches; the unimplemented dies loudly. Input that crossed a trust boundary is never trusted and never panics the kernel — it is refused.
- **The kernel never crashes from userland.** A kernel bug crashes loudly; a userland bug never reaches it.
- **Rust is first class.** Not POSIX, not C. Unrepresentable is best: prefer compile-time safety over runtime checks over tests.
- **Development ergonomics above all.** Iteration speed beats feature count; tooling comes first.

## Architecture

> A snapshot, deliberately shallow — always read the code.

**Kernel** — minimal; new additions are discussed and justified. Resource management, scheduling, process lifecycle, filesystem, device arbitration. 2 MB pages, demand paging, PIE binaries, full SMP.

**Userspace daemons** — compositor, netd, soundd, sshd, logd. Each claims a device or capability from the kernel and serves its function; crash one and the kernel is fine.

**The log is a userland file.** `/bin/logd` reads records on a cursor and owns `/log`; the kernel keeps the record ring, the console and the panel, and writes no file. `SYS_FSYNC` reaches the device's cache flush because logd's durability claim rests on it.

**Syscall ABI** — `toyos-abi/`: struct layouts, syscall numbers, typed wrappers; completely unstable, read the code. Never add or change a syscall without discussion; a deleted syscall's number is retired, never reused. `toyos/` builds on it with typed handles, IPC framing, ports, namespaces and `surface` — userland uses `toyos`, the kernel uses `toyos-abi` only.

**Capabilities** — a process holds exactly what its parent moved into it, and among kernel objects there is nothing it can name to get more. No registry, no connect-by-name, no pid-as-authority: `/bin/init` builds every program's namespace and device claims from `system.toml` before spawning it, and a handle a process does not hold is a bug in that process — the kernel ends it rather than answering a word it can ignore. **The filesystem is the declared exception** (owner ruling 2026-08-20): paths are ambient, `/boot`'s mount guard is the one restriction the ambient space carries, and the full intentional ambient set is the capability end-state track's committed answer.

**CPU state** — a CPU's control registers come from one declaration, applied by the BSP and by every AP and asserted on each; no read-modify-write decides what either holds.

**Input** — the kernel delivers key *transitions*, never what one types; a surface turns one into the other. Translation, layouts, dead keys and escape sequences live in userland, one translator per surface.

**POSIX** — the kernel ABI and SDK are Rust-native and capability-shaped. POSIX lives in `userland/libc` (ours, not a fork) with explicitly relaxed rules. That layer may be ugly; the kernel may not.

## Dependencies

Only **Rust** and **QEMU** (for development). The rules: no binary outside those two — a macOS binary is a hard no, and "only for tests" does not soften it; only general and widely used crates — one that does *our* job we write ourselves, and a driver crate never; no Python; a fork is the sanctioned form of every third-party source. The north star is **self-hosting**: nothing — build, test, or verification — rests on a host binary. Ask of anything new: could this ever run inside ToyOS?

The bar is not yet the tree. The standing failures are declared rather than removed — Python via `rust/x`, `cc` for every host link, four macOS FAT tools. `NOTICE` names every committed third-party file with its hash, upstream and licence; an image carrying `DOOM1.WAD` may not be sold.

- **toyos-ld** — custom linker for bootloader, kernel and all userland. Its output is reproducible, and the container types say so: anything iterated into the output is a `BTreeMap`/`BTreeSet`; a container asked only for membership stays hashed.
- **toyos-cc** — minimal C compiler; exists to bootstrap tinycc and compile doomgeneric, not to grow. A layout or linkage construct it does not implement is refused by name — dropping one silently is a miscompilation.
- **rust/** — Rust compiler/std fork with ToyOS platform support (submodule). Auto-bootstraps; kept current with upstream. Its rules: `src/forkcheck.rs`'s module header.

## Build & test

The testing rules live where they are enforced: instruments and known reds in `src/redlist.rs`, tiers in `src/tiers.rs`, the PR gate and the nightly in `.github/workflows/`. Operationally:

- `cargo run` builds everything (toolchain, kernel, bootloader, userland, initrd) and launches QEMU; `--build-only` skips the launch. `cargo test` runs the QEMU harness; `cargo test --workspace --exclude toyos-build` runs every host-crate suite.
- **Agents verify through `cargo test`, never `cargo run`** — the run path opens a QEMU window on the owner's desktop by design; the harness runs headless.
- **Both produce large output**: run them in the background and read the output file — `[N characters truncated]` means data was lost. ToyOS is fast: a full boot is under a second, incremental builds finish in seconds.

## Repository layout

```
src/               Build system (the root cargo project, package name: toyos-build; its Cargo.toml is also the host workspace, and a gate reds on a crate that joins neither members nor exclude)
kernel/            Kernel
kernel-loom/       Loom models of the kernel's lock-free concurrency, beside the kernel and not in it
toyos-userbound/   Every decision the kernel makes about the user/kernel boundary, pure
toyos-elide/       Log elision decisions, pure
bootloader/        UEFI bootloader
userland/          All userland programs
toyos-abi/         Kernel ABI (types, constants, syscall numbers, syscall wrappers)
toyos/             Userland SDK (typed handles, IPC, ports, namespaces, surface, shm, net)
toyos-manifest/    The one definition of `/etc/system.manifest`
toyos-wallclock/   The calendar, and the zone offset userland has to recover — pure
toyos-keymap/      Layouts, dead-key composition, key translation, layout detection
toyos-fat32/       FAT32 driver, read + write; no format path by design
toyos-fat32-check/ FAT32 checker from Microsoft's fatgen103 — the outside judge
toyos-elf/         ELF64 decoding (no_std, no alloc, forbid(unsafe_code))
toyos-gpt/         GPT parser (no_std, no alloc, forbid(unsafe_code))
toyos-hda/         HDA codec decoding and output-path selection, pure
toyos-mixer/       The mixer's decisions — samples, gain, dither, quantize — pure, corpus-certified
toyos-pci/         MSI and MSI-X capability decoding, pure
toyos-desktop/     Every decision the compositor makes, pure
toyos-ld/          Custom linker
toyos-cc/          Custom C compiler
rust/              Rust compiler/std fork (submodule)
tests/             Integration tests (QEMU-based)
issues/            The issue tracker: one file per issue, typed by kind — see its README
system.toml        What to build and boot
```

## Workflow

**One agent, one worktree, one branch.** `cargo run -- --worktree add <path>` makes one; never `git worktree add` by hand — the naive path clones the rust fork's history and takes the machine-global toolchain name from every other checkout. The primary checkout is not a workspace: it owns `rust/`, the rustup link and `main`; `cargo run -- --sync` moves it onto whatever GitHub merged.

- Stay on the current task. File what you find in `issues/` and do not go fix it; one file per issue, its README has the shape.
- If something blocks, stop and report it. Don't work around it.
- Never degrade audible or visual quality — even temporarily, even for a big win elsewhere — without the owner's explicit sign-off.
- **Never truncate command output.** No `| head`, `| tail`, `| grep` to reduce it; long output runs in the background and is read from the file.
- **Always be empirical.** Read actual output; run the code; investigate root causes instead of guessing.
- **Every written number comes from a command that was run.** An estimate or datasheet bound says so. Write commit messages with `git commit -F <file>`, never `-m` — a double-quoted `-m` substitutes backticks and the shell runs them.
- **Commit freely on your branch; land through a pull request.** `main` moves only through a merged PR, and `cargo run -- --pr` is the whole local half. `gh pr create --draft` at the first push — CI runs on PRs and nothing else; `gh pr ready` plus a written `--title`/`--body-file` when finished (never `--fill`); `gh pr merge --auto --merge`; `cargo run -- --sync` after it lands. Never merge into `main` by hand. Since 2026-08-20 the repository is organization-owned and `main` carries a required **merge queue**: the queue builds each merge's exact composition and runs the required checks on it before `main` moves, `gh pr merge --auto --merge` enqueues, and nothing refreshes by hand. `gate-stage` reads the protection back and reports the regime; the eased-law interlude between the strict era and the queue, its measured threshold, and the breach that ended it are the tracker's record. The PR's title and body become the merge commit's: write them as main's record. A modify/delete conflict is resolved by accounting for every hunk of the modified side, never by checking its headings survived. A merge that deletes a document also deletes every citation to it in the same merge, checked by searching the bare name as well as the path. An ABI change lands on its own PR first; `Abi-Inseparable: <why>` declares the split that genuinely cannot be made; and one ABI-bearing task holds the machine at a time — two lawful sysroot claimants thrash each other's batteries (learned 2026-08-20). Every merge leaves `main`'s tip compiling.
- **Never rewrite history, and never touch `main`.** No `--amend`, no `rebase`, no `--force` — on your own branch as much as anywhere: a pushed hash may already be cited. `main` is protected — PR required, no force-push, no deletion, no bypass.
- **A red is known only if `cargo run -- --known-red <test>` says so** (`src/redlist.rs`). A PR red not about the author's diff is adjudicated there and fixed at its owner, never re-run away.
- **A high-risk change names its two checks.** Security boundaries, the scheduler, the ABI, filesystems, devices, memory management, concurrency primitives: the PR names the negative control or mutation that fails if the implementation is wrong, and one epistemically independent oracle — an external specification, a differential implementation, real hardware, a third-party checker, a formal model, or a recorded real failure. A second agent is not independence: five artifacts from one wrong model still agree.
- **Host load is not an excuse.** A load-coincident audio failure is investigated as a real defect, never re-run away as noise; evidence against that assumption goes to the owner, not into quiet workarounds.
- **Subagents wait in the foreground.** Background-task notifications do not reliably re-wake subagents: run long commands in the foreground with an explicit `timeout`, and for longer work background once and block with a few long foreground waits — never hundreds of polls. Always poll before sleeping.
- **An agent never waits on CI.** It arms auto-merge, reports, and exits — a shell held open sleeping between checks is a stuck slot that looks like work. Sequencing across landings belongs to the orchestrator, done in passes on its own wake-ups; when several finished branches queue behind one another, they land as one batch PR rather than as one agent babysitting N cycles. (Learned 2026-08-19: five green PRs and a waiting agent, and nothing moved for an hour.)
- **Subagents get an explicit model, never the session default.** The orchestrator scopes, dispatches and verifies; it does not hand-work. Match the tier to the judgment in the task, from current knowledge of the model lineup: judgment-bearing coding gets a frontier model, mechanical execution from an exact brief a mid tier, non-coding mechanical work the cheapest, and a trivial edit no agent at all. Never encode a temporary usage circumstance as a rule.
- **Durable facts go in the module header at the site — never in private agent memory, and almost never in a `CLAUDE.md`.** A `CLAUDE.md` is pointers and caveats of the most general kind — it never cites an individual issue file, because it is not an issue tracker; **an agent never edits one.** A rule that truly has no better home and whose violation is invisible or unrecoverable is *proposed as one sentence in the final report*, and the orchestrator places it or declines. The story of a change goes in its commit message; after each task, audit the module header that owns what you changed.

## Planned work

Staged work is an issue like everything else: `rg -l 'kind: track' issues/` lists every open track.
