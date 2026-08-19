---
status: open
kind: finding
opened: 2026-08-08
---

# One atomic read-modify-write per log line cost 350 ms of boot

Measured 2026-08-08, interleaved A/B in one session, `xhci_slow_connect`'s own
boot line as the instrument:

| kernel | `Boot: complete` |
| --- | --- |
| `main` | 497, 497, 498, 501, 503, 504 ms |
| `main` + one `WRITTEN.fetch_add(n, Release)` in `log_ring::append` | 812, 816, 817, 826, 832, 839 ms |
| the same, as a load + store under the lock it already holds | 498, 500, 500 ms |

The `fetch_add` is **outside** the byte loop — once per `write_chunk`, a few
hundred times in a boot. It is a single `lock xadd` on an uncontended line, and
on real hardware it is tens of nanoseconds. Under TCG it is not one instruction:
QEMU cannot always emit an inline host atomic for a guest RMW and falls back to
leaving the translation block to run it exclusively, which is hundreds of
microseconds each. A few hundred of those is a third of a second.

**Why it is worth an entry rather than a comment.** The first A/B said the
regression was 200 ms, the second said 350, and a *third* build — the same
source with a timing `log!` added to `boot_checkpoint` — measured 500 ms and no
regression at all, because the extra call changed inlining enough to move the
cost somewhere the instrument could not see it. So an instrumented build
disproved the defect that the uninstrumented one reproduced 5 times out of 5.
Bisect the **source** when that happens, and interleave the arms: the first
uncontrolled A/B here ran all of one arm and then all of the other, and the
host settled in between, which made a reproducible 350 ms regression look like
host noise.

Nothing else in `log_ring`'s hot path does an RMW — `OWED`, `FILE_OWED` and the
cursors are all plain stores under `RingGuard`, and the comment there now says
why. `DROPPED_BYTES` and `FILE_DROPPED` are `fetch_add`s, but only on the
overflow path.
