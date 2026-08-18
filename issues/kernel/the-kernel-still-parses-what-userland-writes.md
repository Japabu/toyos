---
status: open
kind: track
opened: 2026-08-14
---

# The kernel still parses what userland writes, and the loader move has a deadline

Parked 2026-08-14 by the owner after an external architecture review, to be
taken up when a pipeline slot frees. The direction is his standing one: a
smaller kernel is a more secure kernel, and the answer to a critique is a
deletion, not a defence.

**Move 1 — the userland loader.** Relocations, symbol resolution, TLS layout and
the dlopen family run on attacker-supplied bytes in privileged code. It is not
the largest such parser in Ring 0 — filesystem and USB are both larger — but it
is the only large one whose input an unprivileged process writes byte by byte,
and it holds the most privileged `unsafe`. No production kernel dynamic-links
userland. The end state: the kernel maps the `PT_LOAD`s of a static-PIE image
and jumps; everything else runs in a Ring 3 loader inside the target's own
address space, holding only the file handles it was endowed, where a crafted
binary can only corrupt itself. Five syscalls retire, numbers never reused, and
the crafted-ELF corpus becomes the negative gate against a kernel that no longer
parses most of it.

The evidence that it is worth doing: nine dated commits over eleven days fixed
userland-reachable kernel defects in this code, six of them machine-wide panics;
seven of the loader's twelve bounds are over quantities a workload sets and two
have no bound at all; and two of those ceilings are *already* exceeded by
artifacts this tree builds.

**It has a deadline.** Nothing shipped is dynamically linked today, so the move
is pure deletion. The day `hosted-rustc` turns on, a very large shared object is
dlopened into a kernel whose cache never evicts, and every one of those bounds
becomes load-bearing at once. Do it after the completion architecture lands and
before that day. Independent of everything else; may run as soon as a slot frees.

**Move 2 — filesystem daemons**, sequenced after the completion architecture. A
crafted image attacks the kernel rather than a sandboxed daemon. The FS daemon
needs the blocking story to be efficient, and the boot path needs its story told
first: the initrd covers early boot, but what mounts `/boot` and `/home`, and
with what authority, is the open question.

**Move 3 — the panic-time symbol resolver**, small and independent. ELF parsing
is already a pure crate that forbids unsafe and is tested against a crafted
corpus, and the symbol machinery moved onto it in the 2026-08-15 consolidation —
so what is left of this move is one decision: `rustc-demangle`'s standing. It is
an unforked crates.io dependency, ~2k lines of third-party string parsing in
Ring 0 on the panic path. Fork it into the estate like every other third-party
source, or record the exemption deliberately.

**Small trim to evaluate:** main-thread exit killing the process is kernel policy
the review called unnecessary. A process could end when its last thread does, or
only by explicit exit; the main thread stops being special either way.

**Non-moves, recorded so nobody re-proposes them.**

- **2 MiB pages stay.** The fragmentation is a memory cost, not attack surface,
  and the smaller mm it buys is itself security. Revisit only on measured memory
  pressure, never for this.
- **Invalid handles keep killing the caller.** A process naming a handle it does
  not hold is buggy or probing, and the kill stops handle-guessing cold.
  Compatibility lives in `userland/libc`, which tracks its own fds.
- **The lifetime models are not unified in place.** They shrink as their
  subjects leave the kernel.
