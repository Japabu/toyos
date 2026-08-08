---
status: open
kind: defect
opened: 2026-07-30
---

# Untrusted-input panics that remain

CLOSED and kept for the residual: **`SYS_READDIR` over a large enough tmpfs
directory** was the cheapest one on this list — `Vfs::list` built a
`Vec<(String, u64)>` with one entry per file and no cap, and its 32,769th
`push` doubled the buffer to 65,536 entries, 2,097,152 bytes, past
`mm::MAX_HEAP_ALLOC`. 1.8 s, `fs::write` in a loop, no privilege. Bounded at
`vfs::MAX_LIST_ENTRIES` (16,384) and refused with `ResourceExhausted`;
`readdir_bound` is the gate.

**The residual is that the bound is on the *mount*, not the directory**, and it
has to be: `FileSystem::list` returns every name in the mount and `Vfs::list`
filters, because there is no per-directory index anywhere in the VFS. So a
tmpfs with 16,385 files cannot list any directory in it, including an empty
one, and every `readdir` is O(mount). The fix for that is a real directory
index, not a bigger constant.

**And `bcachefs` is still unbounded underneath it.** The trait takes the limit
so an implementation can refuse *before* it allocates; `TmpFs` does.
`BcacheFsAdapter` and `ReadOnlyBcacheFsAdapter` check the result instead,
because `bcachefs::Mounted::list` has no count primitive and
`btree::collect_all` materialises every entry first. Their refusal is uniform;
their allocation is not bounded. `/home` is writable by userland, so this is a
live path — still open for the `bcachefs` owner. `Node::parse` no longer reserves
from an on-disk count, but `collect_all` still materialises the whole tree.

CLOSED: **`SYS_SYSINFO`'s per-thread `Vec`** was the same shape one syscall
over — one 24-byte entry per live thread, sorted, so the caller's buffer bounded
what was *written* and nothing bounded what was built. `MAX_SYSINFO_THREADS`
(65,536, derived against `MAX_HEAP_ALLOC`) refuses with `ResourceExhausted`, and
the vector is reserved exactly from the count that decides the refusal, so there
is no growth-by-doubling overshoot left to absorb. The residual is that the
thread count itself is still uncapped: this bounds the syscall, not the machine.

**The gate is the actuator's, not the bound's.** 65,536 threads is 8 GiB of
kernel stacks and no guest can make them, so `test-heap-ceiling` drops the
constant to 16 and `heap_ceiling` spawns threads until the refusal comes (13, on
that boot) and then joins them and checks it recovers. What runs is the shipped
count, comparison and error return; the number is the only thing replaced.
