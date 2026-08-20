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
     as of `72e1635`, 83 `IORING_POLL_*`, 34 `poll_add_fd`, 7 `into_fd()` and
     132 `.fd()` occurrences outside `userland/libc`, of which the 62 in
     `toyos/src` are the SDK's own;
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
