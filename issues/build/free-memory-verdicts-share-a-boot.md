---
status: open
kind: defect
opened: 2026-08-10
---

# A verdict on machine-wide free memory cannot share a boot with anything that churns 2 MiB pages

`fd_lifetime`'s `kill_releases_ring` takes `free_bytes()` before spawning a
holder, kills it, and requires free memory to come back within 6 MiB
(`tests/toyos-rust-tests/src/bin/fd_lifetime.rs`). `shm_release_reclaims` has
the same shape. Both run on the shared `tests/testcases` boot, in one guest,
beside every other Rust guest binary.

`free_bytes()` is `SYS_SYSINFO` — **the whole machine's** free physical memory,
not this process's. So the verdict is only sound while nothing else in that
guest is holding or releasing pages across the same window, and nothing
guarantees that: the binaries share a boot and the object layer's release queue
is drained at syscall exit, `do_schedule` entry and the idle loop, which are not
events any of them can order against another's exit.

Observed 2026-08-10 on `wt/toyos-endow`: red once in three full runs with

```
a killed process kept 16777216 bytes of its io_urings
```

and green alone both times, which the harness printed itself —
*"it fails only beside other guests, so its `Sched::Parallel` is wrong."* The
run stays red on the classification, per `tests/CLAUDE.md`.

What changed is only how often it fires. That branch added `handle_basic`,
`handle_transfer` and `kill_while_blocked` to the same boot, and all three churn
pipes — one kernel pipe is exactly one 2 MiB page — and shared-memory regions.
Sixteen megabytes is eight of them. The three gates are not wrong to do it;
a whole-machine measurement is wrong to be a verdict beside them.

## It fires more often since `/bin/logd`, and the mechanism is the entry's own

**2026-08-15, `wt/toyos-logd56` at L6 of `specs/log-architecture-spec.md`.** The
shared boot gained one more process — `logd`, which every image now starts — and
this went from "red once in three full runs" to red in the parallel phase of
**two of two** twelve-wide dev-host runs, green alone both times with the same
sentence the harness prints itself:

```
a killed process kept 14680064 bytes of its io_urings
```

Nothing about the measurement changed and nothing about the object layer's
release queue changed. What changed is the number of things in that guest
holding and releasing pages across the window, which is exactly what the
paragraph above says the verdict cannot survive: logd holds an `io_uring` for
the whole boot, a 64 KiB record buffer, and a `File` whose page-cache pages come
and go as it writes.

**A same-session A/B, 2026-08-15, fourteen more twelve-wide suites.** Seven at
`a76ffd0` (this branch's tip before the strict-review fixes) and seven at
`19ce5d0` (after them), back to back on one host: **0 of 7 and 4 of 7**, every
one ALONE GREEN. Two earlier sevens on the same trees gave 1 of 7 and 2 of 7. So
the rate is somewhere between nought and four in seven on one tree in one
afternoon, which is the strongest statement this instrument supports and is
itself the finding — nothing in the diff between the two arms touches page
churn, the object layer or the release queue (its kernel half is comment text
plus one function with no callers deleted). What moves is how many pages the
*other* binaries in that guest happen to hold across the window, which is what
this entry says the verdict cannot survive.

**`va_exhaustion` joined it on the same runs** — `Sched::Parallel`, red in one
twelve-wide phase with `QEMU disconnected` and *"the guest did not survive
exhaustion"*, green alone, `62 mappings` and the kernel intact. It is a
different binary and the same shape: a verdict about how much of a resource the
machine has, taken while other guests are competing for the host's.

Neither is re-classified here, and that is deliberate: `Sched::Serial` for the
memory verdicts is one of the two shapes below, the second one is better, and
this branch has no standing to pick between them for a defect it only made
louder. **`Instrument::Ci` cannot see any of this** — one guest per machine,
`--jobs 1` — so nothing here is a claim about a CI red.

## What to do

Not "make the margin bigger": a margin that absorbs another binary's working set
absorbs the leak too, and the non-vacuity arm above it (*"an instrument that
cannot see 16 MiB leave cannot see it come back"*) is the reason the margin
cannot simply grow.

Two honest shapes:

- **Give the two memory-verdict binaries a boot of their own**, which is what
  `Sched::Serial` means one level up and what `readdir_bound` already has for a
  weaker reason (it fills `/tmp`). `RUST_SKIP` plus a `MACHINE_TESTS` entry is
  the existing mechanism.
- **Or make the verdict per-process rather than machine-wide.** The object
  census (`toyos::census::Census`) already answers per kind and is immune to
  another binary's churn; what it cannot see is *pages*, which is the whole
  point of these two. `SYS_PROCESS_STATS` reports per-process accounting and
  takes a handle now, so a holder's own page count is askable — that is the
  measurement these tests actually want.

The second is better and is not this branch's to build.
