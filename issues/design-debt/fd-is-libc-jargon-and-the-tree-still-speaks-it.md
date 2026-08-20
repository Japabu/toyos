---
status: open
kind: defect
opened: 2026-08-19
---

# "fd" is libc jargon, and the rest of the tree still speaks it

> **Where this stands, 2026-08-20.** **PR-B is done** — the tree-local half:
> `kernel/`, `userland/` outside libc, `tests/`, `src/` and the pure crates,
> including `with_fd_owner_data` → `with_process_data` (70 sites),
> `io_uring::remove_fd` → `cancel_by_source`, `fd_lifetime` →
> `handle_lifetime` and `abuse_fd_table` → `abuse_handle_table` (both carrying
> the `UNMEASURED` marker), and every stale `MAX_FDS` / `fd::*` /
> `build_child_fds` citation.
>
> **PR-A is not started and is blocked on the owner**, unchanged: `toyos-abi`
> and `toyos` are consumed by three fork repositories, so their half is a
> four-repository landing and a linked worktree cannot do the first step of it.
> Until it moves, the whole SDK surface — `Connection::fd`, `poll_add_fd`,
> `into_fd`, `inherit_fd`, `acceptor_fd`, `client_fds`, `IoUringSqe::fd` — and
> the two registered names `abuse_io_uring` and `io_uring_cancel_wakes` keep
> the word. Two citations PR-B could not repair for the same reason:
> `toyos/src/poller.rs:189` still names `remove_fd`, and `kernel/CLAUDE.md`
> and `userland/CLAUDE.md` both still say it (an agent does not edit one).

Owner ruling, 2026-08-19: **"fds belong only in libc jargon."** The kernel has
no file descriptors — `kernel/src/fd.rs` is deleted, `toyos-abi/src/handle.rs`
is the vocabulary, and a process holds typed handles. POSIX's integer fd is the
interface of exactly one layer, `userland/libc`, and the word is correct there
and nowhere else.

The tree still says it elsewhere. Found by reading, not by a sweep — the sweep
is this issue's work:

- **`fd_lifetime`**, the registered test, with `/bin/test_rs_fd_lifetime`, a
  service named `fd-lifetime-service` and paths `/tmp/fd-lifetime.txt`. Its own
  module header already speaks the new language — "What a handle holds is
  released when the *last* handle goes" — and its body mixes idioms line by
  line: `dup a file fd` a few lines above `dup an acceptor handle`, both
  calling the same handle-taking syscall.
- **The spawn path's "fd map" phrasing**, including the title of the open owner
  question about a spawn skipping a handle it cannot resolve.
- Whatever else `rg -wi fd` finds outside `userland/libc` — names, comments,
  strings. Nobody has counted; the sweep does.

## One wave with the `iouring` rename

`issues/kernel/every-wait-in-this-kernel-is-a-spin.md`'s ABI carries the other
half of the same defect: the string "iouring" names a Linux mechanism this
kernel does not implement (owner has already chosen `inbox`). Both are a prior
architecture's vocabulary outliving the architecture, both are mechanical once
ruled, and each renamed *registered test* costs the same two CI cycles
(`UNMEASURED`, then the measured run) — one wave prices that once.

## The closing check is mechanical

After the wave, the rule is greppable and stays enforced by inspection rather
than memory: `rg -wi fd` outside `userland/libc` finding anything is a defect.
The wave's last commit should state that check and show it returning nothing.

Two boundaries the sweep must respect:

- **`userland/libc` is exempt**, whole. POSIX is its interface; the layer may
  be ugly by charter.
- **The `toyos` SDK is not exempt.** Its wrappers take handles; a
  POSIX-flavoured convenience name there is the blur this ruling removes. If a
  site genuinely cannot lose the word — an ABI string, a wire format — it is
  named in the wave's report rather than silently kept.

## The sweep, counted (2026-08-20)

Nobody had counted. Measured at `0a7470f`, excluding `rust/`, `target/`,
lockfiles and `tests/testcases/` (third-party C):

| | `rg -wi fd` | wider pattern | `io_?uring` |
|---|---|---|---|
| `toyos-abi/src` | 36 / 4f | 42 / 5f | 36 / 5f |
| `toyos/src` | 115 / 10f | 130 / 11f | 25 / 3f |
| `kernel` + `kernel-loom` | 36 / 11f | 133 / 19f | 225 / 27f |
| `userland/libc` (exempt) | 112 / 11f | 208 / 11f | 2 / 1f |
| `userland` non-libc | 130 / 11f | 203 / 14f | 54 / 11f |
| `tests/` | 107 / 24f | 156 / 38f | 63 / 14f |
| `src/` build system | 9 / 4f | 18 / 5f | 9 / 2f |
| other pure crates | 7 / 2f | 13 / 6f | 7 / 5f |
| `issues/` + `NOTICE` | 41 / 20f | 71 / 32f | 34 / 15f |
| **outside `userland/libc`** | **481 / 86f** | 766 / 130f | **453 / 82f** |

**The closing check is weaker than the ruling, and the sweep has to know it.**
`\bfd\b` matches neither `fd_map`, `ring_fd`, `poll_add_fd`, `PipeFds` nor
`FDs`, because `_` and a trailing letter are word characters. The wider pattern
above is

```
(?i)(^|[^a-zA-Z0-9])fds?([^a-zA-Z0-9]|$)|(?i)(^|[^a-zA-Z0-9_])fds?_|_fds?($|[^a-zA-Z0-9_])|[a-z]Fds?([^a-zA-Z]|$)
```

and it finds 285 lines the closing check does not. Whichever the wave settles
on, the last commit states *that* one.

Sites that keep the word, each because it is not this jargon:

- **OVMF firmware files** — `ovmf/*.fd` is a filename extension: `NOTICE` (6),
  `src/qemu.rs` (2), `tests/common/qemu.rs` (2).
- **`src/buildlock.rs`** — `flock(fd: i32)` on the *host*, through
  `std::os::unix::io::AsRawFd`. A genuine POSIX descriptor, 5 sites.
- **`src/toolchain.rs:2378`** — a rustc diagnostic path,
  `library/std/src/os/fd/owned.rs`, quoted verbatim.
- **`toyos-cc/src/codegen/resolve.rs`** — `fd` is a *field declarator*, 4 sites.
  A different word that greps the same; rename it to `decl` and the check gets
  quieter for free.

Stale citations to delete rather than rename: `tests/common/volumes.rs` twice
names `fd::write`, and `kernel/src/mm/alloc.rs`, `toyos-sched/src/cpu.rs`,
`userland/netd/src/main.rs:1272` and
`issues/isolation/compositor-and-netd-unbounded-accept.md` each cite `MAX_FDS`
— a constant of the deleted `kernel/src/fd.rs`, one of them still giving its
file and its value. The name the kernel refuses by is `MAX_HANDLES`
(`toyos-abi/src/handle.rs`).

## The ABI half is a four-repository landing, and that is the blocker

**`toyos-abi` and `toyos` are consumed by the fork estate, which is not in this
repository** — the blind spot
`issues/build/an-abi-item-only-a-fork-calls-looks-caller-less.md` records for
*deletions* bites a *rename* identically, and this is the live case:

- **`rust/library/std/src/sys/net/connection/toyos.rs`** — `Japabu/rust`, the
  submodule. `use toyos::poller::{Poller, IORING_POLL_IN, IORING_POLL_OUT};`,
  `poller.poll_add_fd(…)` twice, `Pipe::into_fd()` three times, `Pipe::fd()`
  twice. `rust/library/std/Cargo.toml:106` takes `toyos-abi` and `toyos` **by
  path into this tree**, so every sysroot build compiles it against the
  working tree's SDK.
- **`Japabu/mio@toyos`**, pinned at `e8068c2` by `userland/Cargo.lock:1801` —
  `src/sys/toyos/selector.rs` does `use toyos_abi::io_uring::*` and names
  `IoUringSqe`, `IoUringCqe`, `IoUringParams`, `IoUringRingHeader`,
  `IORING_OP_POLL_ADD`, `IORING_POLL_IN`, `IORING_POLL_OUT`, `SQ_RING_OFF`,
  `CQ_RING_OFF`, `SQES_OFF`, `syscall::io_uring_setup` and
  `syscall::io_uring_enter` — 21 uses in one file — plus `Pipe::into_fd()`
  five times. `userland/sshd` pulls it in through tokio.
- **`Japabu/socket2@toyos`**, pinned at `b55ee41` by
  `userland/Cargo.lock:3304` — `src/sys/toyos.rs` uses
  `toyos::poller::IORING_POLL_IN`/`OUT`, `Poller::poll_add_fd`, `Pipe::fd()`
  four times and `Pipe::into_fd()`.

So the ABI/SDK half is not one pull request. It is **the three fork repositories
first, then the monorepo** carrying the submodule bump and the two lockfile
pins — and a linked worktree cannot do the first of them at all, because
`git worktree add` leaves `rust/` an empty stub (`src/CLAUDE.md`). Whether that
sequence is worth its cost is the owner's call, not an agent's.

What is *not* blocked, and is the whole majority of the count: everything in
`kernel/`, `userland/` outside libc, `tests/` and `src/` — 282 of the 481
`rg -wi fd` hits outside libc — names nothing a fork can see. Parameter names
inside `toyos-abi`'s wrappers are invisible too (Rust has no named arguments),
as is `PipeFds`, which no fork spells.

## `inbox` already names two other things in this kernel

`inbox` is not a free word. `completion::Inbox`
(`kernel/src/completion/inbox.rs`, landed with #91) is a *task's* bounded record
ring, with `kernel-loom/tests/inbox.rs` and two cargo features
(`inbox-release-off`, `inbox-signal-as-post`) named after it; and
`ConnectionEnd`/`PortShared` call their receiving `HandleQueue` an `inbox`
beside an `outbox` (`kernel/src/object/service.rs`).

The first is convergence rather than collision — this track's own chunk list
says "**the ring as an inbox**", so the object userland sets up and the record
ring a waiter owns are meant to become one thing. The rename should therefore
put the ABI's `Inbox` where that convergence lands and not invent a third
`Inbox` beside it: `crate::object::inbox` for the object, `completion::inbox`
for the waiter's ring, each header naming the other. The connection's
`inbox`/`outbox` are the common noun and stay.

Left open for the same reason the fork blocker leaves it open: whether `SQ`,
`CQ` and `SQE` go with `io_uring`. They are Linux's acronyms for a submission
and a completion queue, which is what the structure honestly is; the ruling
names the string `iouring` and not these.

## The order the blocker forces

"An ABI change lands on its own pull request first" is the rule, and here the
ABI change cannot be attempted at all until the owner decides on the fork
sequence. So the wave inverts: the monorepo-local half — `kernel/`, `userland/`
outside libc, `tests/`, `src/` — touches no ABI, is not the ABI PR riding along
with unrelated work, and lands whenever. What it may not do is rename anything
that would read as half-done beside an unrenamed ABI: `abuse_io_uring` and
`io_uring_cancel_wakes` keep their registered names until the ABI moves.

Registered names this wave changes, priced once because each costs the
`UNMEASURED` round-trip (`tests/CLAUDE.md`): `fd_lifetime` and `abuse_fd_table`
now — `fd_lifetime` also carries `/bin/test_rs_fd_lifetime`, a service named
`fd-lifetime-service`, `/tmp/fd-lifetime.txt`, `/home/fd-lifetime-killed.txt`,
a row in `tests/test-durations` and **three** `src/redlist.rs` entries —
and `abuse_io_uring` and `io_uring_cancel_wakes` with the ABI half. Four names,
two of them deferred.
