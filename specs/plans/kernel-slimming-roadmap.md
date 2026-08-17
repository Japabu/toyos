# Kernel slimming — the parked roadmap

> Parked 2026-08-14 by the owner after an external architecture review; taken
> up when a pipeline slot frees. The direction is the owner's standing one:
> a smaller kernel is a more secure kernel, and the answer to a critique is a
> deletion, not a defense.

## Move 1 — the userland loader (pipeline 5)

Program loading past the minimum — relocations, symbol resolution, TLS layout
and the dlopen family — operates on attacker-supplied bytes in privileged code.
It is not the largest such parser in Ring 0, as this paragraph claimed until it
was measured: filesystem and USB are both larger, and program loading is third.
What it is instead is the only large one whose input an unprivileged process
writes byte by byte, and the one holding the most privileged `unsafe`. No
production kernel dynamic-links userland.

The end state: the kernel maps the `PT_LOAD`s of a static-PIE image and jumps;
everything else runs in a Ring 3 loader inside the target's own address space,
holding only the file handles it was endowed, where a crafted binary can only
corrupt itself. Five syscalls retire, not the three this paragraph first named
(numbers never reused). Spec first, house style; the crafted-ELF corpus becomes
the negative gate against a kernel that no longer parses most of it.

**Sized and decided by `specs/assessments/2026-08-17-move1-loader-scoping.md`,
which is the arbiter for every number above.** The move is worth doing on
evidence the paragraph above did not have: nine dated commits over eleven days
fixed userland-reachable kernel defects in this code, six of them machine-wide
panics; seven of the loader's twelve bounds are over quantities a workload sets
and two have no bound at all; and two of those ceilings are *already* exceeded
by artifacts this tree builds. **It has a deadline.** Nothing shipped is
dynamically linked today, so the move is pure deletion — the day `hosted-rustc`
turns on, a very large shared object is dlopened into a kernel whose cache never
evicts, and every one of those bounds becomes load-bearing at once. Do it after
the completion architecture lands and before that day.

Independent of the completion architecture; may run as soon as a slot frees.

## Move 2 — filesystem daemons (pipeline 6, after completions)

bcachefs and FAT32 parsing are the second-largest untrusted parser in Ring 0:
the tree already treats every disk as untrusted input, but the parser still
runs privileged, so a crafted image attacks the kernel rather than a sandboxed
daemon. The log architecture already moved `/log` file writing and policy out
to logd — this is the same direction for the rest.

Sequenced after the completion architecture: an FS daemon needs the blocking
story to be efficient, and the boot path needs its story told (the initrd
covers early boot; what mounts `/boot` and `/home`, and with what authority,
is the spec's first question).

## Move 3 — symbol machinery onto the pure crate (small, independent)

**Partly overtaken: verify what is left before planning this.** Wave A of the
2026-08-15 consolidation audit moved the symbol machinery onto `toyos-elf` and
retired the bootloader's second decoder, so the premise below — that
`kernel/src/symbols.rs` and a sibling hold their own symtab walks — was false as
of 2026-08-17. What remains of this move is the `rustc-demangle` question in the
paragraph after it, which stands untouched.

ELF parsing is already `toyos-elf/` (pure, `forbid(unsafe_code)`, crafted +
real corpus). But `kernel/src/symbols.rs`'s panic-time resolver re-implements
the symtab walk over raw pointers, bypassing the hardened crate exactly where
the input is least trusted; it and `kernel/src/loader/symbols.rs` (463 lines
together) have no host tests, only end-to-end guest gates. Pull the scan onto
`toyos-elf`'s symbol module or a pure sibling, host-test it against the
existing crafted corpus, and add the property that arbitrary bytes make it
answer or decline, never panic — the no-alloc, no-lock constraint travels with
it as a type-level property, not a comment.

In the same pass, decide `rustc-demangle`'s standing: an unforked crates.io
dependency, ~2k lines of third-party string parsing in Ring 0 on the panic
path. Fork it into the estate like every other third-party source, or record
the exemption deliberately where the dependency rules live.

Small enough to run any time as one PR.

## Small trims

- Main-thread exit killing the process is kernel policy the external review
  called unnecessary. Evaluate: a process ends when its last thread does, or
  only by explicit `SYS_EXIT`; the main thread stops being special either way.

## Non-moves, recorded so nobody re-proposes them

- **2 MiB pages stay.** The fragmentation is a memory cost, not attack
  surface, and the smaller mm it buys is itself security. Revisit only on
  measured memory pressure, never for this roadmap.
- **Invalid handles keep killing the caller.** A process naming a handle it
  does not hold is buggy or probing, and the kill stops handle-guessing cold.
  Compatibility lives in `userland/libc`, which tracks its own fds and
  synthesizes `EBADF` without passing garbage down.
- **The lifetime models are not unified in place.** They shrink as their
  subjects leave the kernel; a unification refactor belongs to the
  memory-ownership track, not here.

## Sequencing

Log architecture (in flight) → completions → loader → filesystem daemons.
Move 3 and the small trims fit between any two, one PR each.
