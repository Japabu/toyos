---
status: open
kind: defect
opened: 2026-08-19
---

# "fd" is libc jargon, and the rest of the tree still speaks it

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
