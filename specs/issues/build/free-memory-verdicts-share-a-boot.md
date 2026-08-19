---
status: open
kind: defect
opened: 2026-08-10
---

# A verdict on machine-wide free memory cannot share a boot with anything that churns 2 MiB pages

> **The diagnosis this heading carries was wrong, and the section
> *"It is not the neighbours: the release outlives the syscall that caused it"*
> is what replaced it.** The two binaries do share a boot with page churn and
> that is still true; it is not what was making them red. The 2026-08-10 and
> 2026-08-15 readings are kept as what was believed and on what evidence,
> because the new one has to account for the same measurements.

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

**2026-08-15, on `wt/toyos-logd56`, the branch that moved `/log` to userland.** The
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

## What was proposed on that reading

Recorded because the section *"It is not the neighbours: the release outlives
the syscall that caused it"* rules the first of the two out on evidence, and it
should not be re-proposed:

- **Give the two memory-verdict binaries a boot of their own** (`Sched::Serial`
  one level up, `RUST_SKIP` plus a `MACHINE_TESTS` entry). **This would not have
  worked**: the failure reproduces in a guest running one binary, so a boot of
  its own is the condition it fails under.
- **Or make the verdict per-process rather than machine-wide**, through the
  object census or `SYS_PROCESS_STATS`. Still a better instrument for what these
  two want to say, and still not built — but it would not have fixed this
  either, because a per-process page count read at the same moment is early for
  the same reason a machine-wide one is.

Neither is "make the margin bigger", which stays wrong for the reason it always
was: a margin that absorbs another binary's working set absorbs a leak too, and
the non-vacuity arm (*"an instrument that cannot see 16 MiB leave cannot see it
come back"*) is why it cannot simply grow.

## It is not the neighbours: the release outlives the syscall that caused it

**2026-08-19, on `wt/toyos-fdleak` at `8e9f851`.** CI run `32237424649` put both
of this entry's discriminators out of reach: `fd_lifetime` went red on
`Instrument::Ci` — one guest per machine, `--jobs 1` — and the harness's own
`ALONE` re-run, which is a fresh boot carrying that binary and nothing else,
was **red again on the same failure**. `main` itself was red at `8e9f851` on
`ci` and on `gate A, thorough`. A shared-boot artifact cannot do that.

What the 2026-08-10 and 2026-08-15 readings got wrong is narrower than it looks.
The mechanism they named was never host contention — this entry's own opening
says *"in one guest, beside every other Rust guest binary"* — so `Instrument::Ci`'s inability
to see contention was never the right shield, and the sentence claiming it was
is withdrawn. But the neighbours are not the cause either.

**Twenty kill rounds, alone in the guest, two CPUs, TCG.** The one binary in the
boot, `kill_releases_ring` looped, reading `SYS_SYSINFO` eight times back to back
after each `wait`. The deficit against that round's pre-spawn reading, in
megabytes:

```
round 1  [12, 10, 10,  8,  6,  6,  6,  6]
round 3  [12, 10, 10, 10,  8,  6,  4,  2]
round 5  [10, 10,  8,  6,  4,  2,  2,  2]
round 9  [14, 12, 10, 10, 10,  8,  6,  4]
round 13 [14, 14, 12, 10, 10, 10, 10,  8]
```

Ten of the twenty rounds decayed like that and the other ten read zero on the
first try — so **the failure this entry is about fires at about one round in two
with no neighbour in the guest at all**, and `Sched::Serial` would not have
touched it. The staircase is 2 MiB at a time, which is one io_uring ring page.

**Nothing leaks.** Across the same twenty rounds free memory came back to the
round-0 baseline every time; the drift was zero at every round. Twenty separate
`ALONE` runs of the unmodified binary on the same host were green twenty times,
which is why the single-shot rate is lower than one in two and why the dev host
kept saying `ALONE … GREEN`.

The cause is `object::drain_zero_handles`: it clears `ZERO_PENDING` before it
runs any hook, so a batch another CPU has taken is indistinguishable from an
empty queue, and the syscall that dropped the last handles reaches its own drain
site, is told there is nothing to do, and returns with its objects unreleased.
A kernel trace shows all eight `RingRef` frees landing in a drain that runs
after `kill_process` has returned, and a second CPU taking a batch mid-kill.
That half is `specs/issues/kernel/deferred-release-outlives-its-syscall.md`, and
it is not free-standing work — the release protocol is
`specs/completion-architecture-spec.md` §21 row 9's.

**What was done here.** Both binaries now read free memory once the machine has
stopped giving it back: samples 10 ms apart until two agree, bounded at a
hundred, the last reading handed back either way. It is a liveness bound and not
a margin — a kernel that frees nothing is quiescent on the first pair and reds
at once, so neither assertion lost a tooth. `kill_releases_ring` and
`shm_release_reclaims` both take theirs that way.

**What is left open.** The heading's claim is still true as a claim: these two
verdicts *are* machine-wide and they *do* share a boot with page churn. Nothing
has measured how much of the 2026-08-15 four-in-seven was this race and how much
was the neighbours, and the settle makes both quieter at once, so that split is
now unmeasurable on this instrument. The per-process measurement stays the
better instrument and stays unbuilt.
