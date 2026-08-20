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
> **PR-A is designed and staged, and its finale has not landed.** The owner
> released it on 2026-08-20 ("2 go", plus "3 rename": SQ/CQ/SQE become plain
> words). The naming table and the landing order are below; the two fork
> branches are pushed and deliberately do not compile until the finale lands.
> Until it does, the whole SDK surface — `Connection::fd`, `poll_add_fd`,
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

**He made it on 2026-08-20: go.** The inventory above stands and is what the
two sections at the end of this file build on, with one correction it earns by
being re-measured against the whole fork estate rather than the three
repositories already known to be in it. Six `userland/Cargo.lock` entries
depend on `toyos`/`toyos-abi` from outside this repository — cpal,
getrandom ×3, libloading, mio, socket2 — and only mio and socket2 name
anything the rename touches: getrandom calls `syscall::random`, libloading
calls `dl_open`/`dl_sym`/`dl_close`, and cpal calls `audio::AudioStream` and
`futex_wait`/`futex_wake`. The estate therefore costs two fork branches and
not five, and both are pushed: `Japabu/mio@toyos-inbox` at `c84d7e3` and
`Japabu/socket2@toyos-inbox` at `19eb798`.

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

**Ruled 2026-08-20: rename.** They go, and what replaces them is the thing the
paragraph above already says they honestly are, spelled out — submission ring,
completion ring, submission entry. The words are in the table below, and this
section's recommendation is what that table adopts: the ABI's inbox goes to
`crate::object::inbox` and the waiter's ring stays `completion::inbox`, which
is also why the SDK's `Poller` keeps its name rather than becoming a third
`Inbox` in userland.

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

**The deferred two move once more, by one PR.** "With the ABI half" is right
about the order and wrong about the vehicle: an `UNMEASURED` round-trip inside
the finale would spend a CI cycle of the one landing that holds the machine's
sysroot still. So they ride the tree-local PR that follows it — the same PR
that renames the kernel's internal `io_uring` vocabulary — where a red is about
a name and not about a sysroot. Order unchanged; cost moved off the critical
window.

## PR-A: the naming table

Every name below is derived from a word the tree already uses, not invented.
`inbox` is the owner's choice for the mechanism; `Watch`, `Token`, `Submission`
and `Completion` come from `kernel/src/completion/` — the architecture these
names are joining — and `raw`/`into_raw`/`from_raw` is already the SDK's
spelling for "the `RawHandle` form".

**`toyos-abi`. The module moves: `toyos-abi/src/io_uring.rs` →
`toyos-abi/src/inbox.rs`, `toyos_abi::io_uring` → `toyos_abi::inbox`.**

| old | new |
|---|---|
| `IORING_OP_NOP` | `OP_NOP` |
| `IORING_OP_POLL_ADD` | `OP_WATCH` |
| `IORING_OP_ACCEPT` | `OP_ACCEPT` |
| `IORING_POLL_IN` | `READABLE` |
| `IORING_POLL_OUT` | `WRITABLE` |
| `IoUringSqe` | `Submission` |
| `IoUringSqe::fd` | `Submission::handle` |
| `IoUringSqe::user_data` | `Submission::token` |
| `IoUringCqe` | `Completion` |
| `IoUringCqe::user_data` | `Completion::token` |
| `IoUringRingHeader` | `RingHeader` |
| `IoUringParams` | `RingLayout` |
| `IoUringParams::sq_off` / `cq_off` / `sqes_off` | `submission_ring_off` / `completion_ring_off` / `submissions_off` |
| `IoUringParams::sq_ring_size` / `cq_ring_size` | `submission_ring_size` / `completion_ring_size` |
| `SQ_RING_OFF` / `CQ_RING_OFF` / `SQES_OFF` | `SUBMISSION_RING_OFF` / `COMPLETION_RING_OFF` / `SUBMISSIONS_OFF` |
| `syscall::IoUringSetup` | `syscall::InboxSetup` |
| `syscall::io_uring_setup` | `syscall::inbox_setup` |
| `syscall::io_uring_enter` | `syscall::inbox_submit` |
| `SYS_IO_URING_SETUP` | `SYS_INBOX_SETUP` — **still 89** |
| `SYS_IO_URING_ENTER` | `SYS_INBOX_SUBMIT` — **still 90** |
| `OBJECT_KINDS[5] = "IoUring"` | `"Inbox"` |
| `syscall::PipeFds` | `syscall::PipeEnds` |
| every `fd: RawHandle` parameter in `syscall.rs` | `handle: RawHandle` |

`OP_WATCH` rather than a transliteration of `POLL_ADD`: the op arms a one-shot
readiness watch, `kernel/src/completion/mod.rs` already calls that a `Watch`,
and `cancel_by_source` already walks "a source's watcher list". `token` rather
than `user_data` for the same reason — `completion::arm` takes a `Token`, and
the SQE field is the same value round-tripping through shared memory.
`inbox_submit` rather than a rename of Linux's "enter": the SDK has called this
operation `Poller::submit` since it was written, and a zero-submission call is
the pure wait it already supports.

**A rename is not a retirement.** `CLAUDE.md`'s rule is that a *deleted*
syscall's number is retired and never reused. 89 and 90 are the same two calls
with the same arguments and the same struct layouts; only the Rust identifier
moves, so neither number is retired and no new number is taken.
`src/sourcegate.rs`'s `RETIRED_ABI_NAMES` therefore does not gain a row for
either — but its doc comment cites `toyos-abi/src/io_uring.rs` by path and that
citation moves with the file.

**`toyos` SDK.**

| old | new |
|---|---|
| `OwnedHandle::fd()` (crate-private) | `OwnedHandle::raw()` |
| `Device::fd()`, `Pipe::fd()`, `Connection::fd()`, `Keyboard::fd()`, `Mouse::fd()`, `Nic::fd()` | **deleted** — `AsHandle::as_handle()`, which every one of these six types already implements with an identical body |
| `Pipe::into_fd()` | `Pipe::into_raw()` |
| `Poller::poll_add()` | `Poller::watch()` |
| `Poller::poll_add_fd()` | `Poller::watch_raw()` |
| `Poller::ring_fd` (field) | `Poller::inbox` |
| `Poller::sq_size` / `cq_size` (fields) | `submission_ring_size` / `completion_ring_size` |
| `poller`'s re-export of `IORING_POLL_IN` / `_OUT` | `READABLE` / `WRITABLE` |
| `surface::Host::acceptor_fd()` | `Host::acceptor_handle()` |
| `surface::Host::client_fds()` | `Host::client_handles()` |
| every `fd: RawHandle` parameter in `ipc.rs` | `handle: RawHandle` |

The six `fd()` methods are the one row where the honest name is a deletion.
Each is a second public spelling of `as_handle()` on the same type with the
same body; renaming it would mint a second word for one thing, which is the
blur this ruling exists to remove. `Poller` is deliberately **not** renamed to
`Inbox`: the recommendation this wave adopts is `crate::object::inbox` for the
ABI object against `completion::inbox` for the waiter's ring, and a third
`Inbox` in userland would undo exactly that separation.

**`rust/library/std`.** std is the POSIX layer here on `userland/libc`'s
charter, so `OwnedFd`, `RawFd`, `as_raw_fd` and `sys::pipe::Pipe { fd }` all
stay. The exception is `os::toyos::*` — ToyOS's own extension API, which is not
POSIX and must speak ToyOS.

| old | new |
|---|---|
| `os::toyos::process::CommandExt::inherit_fd(child_fd, parent_fd)` | `inherit_handle(child_slot, parent_handle)` |
| `sys::process::toyos::Command::inherit_fd` | `Command::inherit_handle` |
| `Command::extra_fds` | `Command::extra_slots` |
| `Command::setup_fd` and its `fd_map` locals/params | `setup_slot`, `slot_map` — matching `SpawnArgs::slot_map_ptr`, which PR-B already renamed |
| `sys::net::connection::toyos::raw_fd()` (returns `RawHandle`) | `raw_handle()` |

## PR-A: the landing order

**The constraint is the shared sysroot, and it is machine-global.** The primary
checkout owns `rust/`, and `rust/library` is the fourth source of the one
sysroot every worktree links against (`src/CLAUDE.md`, "The host's locks and
slots"): a doc-comment change under `toyos-abi/src` already costs a sysroot
claim, and moving the rust submodule's gitlink moves the sysroot's own sources
out from under every other checkout on the machine. So the rust-fork checkout
moves **only in a quiet window with no other worktree building**, and the
window is closed by the finale landing rather than by a timer.

The wave is four repositories: this one, `Japabu/rust` (submodule), and the two
fork repositories that name a symbol in the table. Six lock entries in
`userland/Cargo.lock` depend on `toyos` or `toyos-abi` from outside the
monorepo — `cpal`, `getrandom` ×3, `libloading`, `mio`, `socket2` — and only
`mio` and `socket2` name anything the table renames; `getrandom` calls
`syscall::random`, `libloading` calls `dl_open`/`dl_sym`/`dl_close`, and `cpal`
calls `audio::AudioStream` and `futex_wait`/`futex_wake`. Nothing else needs a
pin bump.

1. **Stage the forks first, and expect them not to compile.** `toyos-inbox` on
   `Japabu/mio` and on `Japabu/socket2` carry the renames against an SDK that
   does not exist yet. They are verified by reading the symbol table against
   this section, and they are compiled for the first time at step 4.
2. **The kernel's *internal* `io_uring` vocabulary is not in this landing.**
   `kernel/src/io_uring.rs` → `kernel/src/object/inbox.rs`, `IoUringObject` →
   `InboxObject`, `RingId`/`RingRef`, and the 21 `io_uring_watchers`
   accessors (65 occurrences of the name) across `pipe.rs`, `keyboard.rs`,
   `mouse.rs`, `net.rs`, `log/user.rs`, `object/port.rs` and the two audio
   drivers are tree-local,
   need no quiet window, and belong to a separate PR after the finale. Keeping
   them out is what makes the stop-the-world PR small enough to land in one
   window.
3. **The two registered tests are not in this landing either.**
   `abuse_io_uring` → `abuse_inbox` and `io_uring_cancel_wakes` →
   `inbox_cancel_wakes` each cost two CI cycles (`UNMEASURED`, then the
   measured run) and each needs its `tests/test-durations` row and
   `tests/toyos.rs`'s per-name timeout moved with it. They ride with the
   kernel-vocabulary PR, where a red is about a name and not about a sysroot.
4. **The finale is one merge**, and it is `Abi-Inseparable` by construction —
   an ABI whose consumers are pinned to the old names compiles nowhere:
   - `toyos-abi` and `toyos` renamed per the table;
   - every forced call site in `kernel/`, `userland/`, `tests/` and `src/` —
     measured on this branch's merge with `edf8d69`, 83 `IORING_POLL_*`, 34
     `poll_add_fd`, 7 `into_fd()` and 132 `.fd()` occurrences outside
     `userland/libc`, of which the 62 in `toyos/src` are the SDK's own;
   - `src/sourcegate.rs`'s citation of `toyos-abi/src/io_uring.rs`;
   - the `rust` submodule gitlink, bumped onto a `Japabu/rust` commit carrying
     the std edits;
   - both fork pins in `userland/Cargo.lock`, moved to the `toyos-inbox` heads,
     with `userland/Cargo.toml`'s `[patch]` branches following.
5. **`cargo run -- --sync` in the primary immediately after**, because until it
   runs the primary's `rust/` and the landed gitlink disagree and every
   sysroot check on the machine reads the difference as an unlanded claim.

A cheap rehearsal exists and should be used before the window opens:
`__CARGO_TESTS_ONLY_SRC_ROOT` type-checks a std edit against a cloned
`rust/library` without touching the shared sysroot (`src/CLAUDE.md`, "Worktrees"),
so every std site in the table is proved to compile before anything global moves.

Two things this plan leaves genuinely open, both for the owner:

- **Deleting the six `fd()` methods** rather than renaming them. It is the only
  row that removes API, and it is the row a reviewer is most likely to want
  back as `handle()`.
- **`src/sourcegate.rs`'s `RETIRED_ABI_NAMES` omits `IORING_OP_CLOSE`.**
  `toyos-abi/src/io_uring.rs:14` records op code 4 as retired with that name,
  and the table that stops a retired name coming back does not carry it. That
  is a pre-existing hole, not one this wave opens, and it wants its own issue.
