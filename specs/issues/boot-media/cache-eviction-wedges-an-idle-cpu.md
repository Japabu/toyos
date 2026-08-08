---
status: open
kind: defect
opened: 2026-08-01
---

# `cache_eviction` wedges or faults on an *idle* CPU after the test has exited

Seen three times in one session on `main` at `b0e69c5`. The in-guest test
always succeeds: `cache eviction ok: 1168 page reads verified`, exit code 0, at
3.6-5.0 s. What fails is what happens afterwards.

- Full-suite run: `KERNEL PANIC: read unmapped address at 0x58` at 3.615 s on
  cpu1, `#PF SKIP: cr2=0x58 rip=0xffff80007d48f396 err=0x0 (no tid, not user)`,
  12 ms after `exit: test_rs_cache_eviction pid=2 code=0`.
- Two of three isolated re-runs: the harness times out after 180 s, with
  `!!! DOUBLE PANIC !!!` on cpu1 `tid=0` at 66.1 s — 61 s after the exit.
- One of three: clean pass.

**Not the page cache's fallible-read change**, which landed just before it.
Every error path that change added logs a line, and `grep` over the failing
run's serial finds zero of `could not be cached`, `serving zeros`,
`write-back .* failed`, `no slot could be freed`. The cache did the 1168 reads
and reported them correct.

The shape points at the idle path rather than at the workload: no current
thread, after the process is gone, on the CPU that is not running the test.
`4a1f898` and `a10c459` put a log sink on the idle loop that writes a file to
the boot stick, which is new code running in exactly that state and reaching a
block device through a filesystem. That is a lead, not a diagnosis — nobody has
symbolized `rip` against the boot's `Kernel memory located at` line yet, and
that is the first thing to do.

**Measured since, and one contributing cause closed at `5bb1193`.** The
per-CPU idle stack was 16 KiB of ordinary heap with **no guard page**, so an
overflow there did not fault — it rewrote whatever the allocator put
underneath, and a `BTreeMap` node with an out-of-range index (seen: `slice::
get_unchecked` in `CpuSched::drain`) or a write to `0x4` is what that looks
like from the scheduler's parked map. **It has one now**: `alloc_idle_stack`
takes a 4 KiB page out of the direct map below every idle stack
(`paging::guard_4k`, which splits the 2 MiB leaf that covers it), so an
overflow faults where it happens and is reported instead of being found later
somewhere else. `idle_stack_guard` is the gate — the guard page is the one
page in the kernel deliberately absent from the direct map, and absence is
invisible to every log line, so `test-idle-guard` supplies the one read that
touches it. Note what it does *not* change: a fault on a kernel address is
fatal by policy either way, so the machine still halts; what is new is that it
halts with a report naming the address. Instrumented at the block layer, with the
USB command path still below the probe, the sink's path used **11,505 bytes of
the 16,384**. Three 4 KiB page buffers accounted for most of it —
`Vfs::flush_file`'s, and `file_cache`'s two miss buffers, which were
`[0u8; PAGE_SIZE]` handed to `Box::new`. Moving all three to the heap took the
high water to **6,209**.

What that does *not* establish: that the overflow happened. 11,505 plus the
xHCI/MSC chain is close to 16,384 but nothing was caught crossing it, and the
A/B is only three runs each way — three clean with `log_file::poll` removed from
the idle loop, three not clean with it. If it recurs at `5bb1193` or later, the
stack is no longer the first suspect and the `rip` symbolization is.
