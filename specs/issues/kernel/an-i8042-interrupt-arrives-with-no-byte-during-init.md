---
status: open
kind: defect
opened: 2026-08-15
---

# An i8042 interrupt with no byte behind it lands during init, and `i8042_undecoded_bytes` reads it as its own

The driver reports `i8042: 1 interrupts and 0 bytes, nothing decoded — first
seen at 459ms` on a boot where nothing has been typed. `i8042_undecoded_bytes`
then fails, because it finds the **first** line containing `nothing decoded` and
asserts that it names the byte it injected:

```
FAIL i8042_undecoded_bytes: the line names no byte:
  [kernel 0.459 cpu1] i8042: 1 interrupts and 0 bytes, nothing decoded — first seen at 459ms
```

## What it is

An interrupt the ISR finds nothing behind. The likely producer is the driver's
own init: it sends commands to the keyboard and **polls** for the answers, while
the GSI is already unmasked — so a byte can be consumed by the polling read
before the ISR that the same byte raised gets to run. The counters are then
honest and the conclusion the line draws is not: nothing was undecodable, the
byte was simply somebody else's.

The stamps are all inside the keyboard bring-up window: **403 ms, 459 ms and
515 ms** over three occurrences.

## Observed

`cargo test`, dev host, 2026-08-15, on `wt/toyos-logd`:

| tree | full suites | reds |
|---|---|---|
| `origin/main` (`4d8c2e9`) | 7 | 0 |
| this branch before the byte ring went (`b8457df`) | 5 | 0 |
| this branch after it (`ee8369c`+) | 5 | 2 |
| the same with the *drain's* interrupts-off window bounded | 5 | 1 |
| the same with **`write_console`'s** window bounded as well (2026-08-15) | 9 | **2** |
| the tip of the same branch, that one window *not* bounded, same session | 5 | **1** |
| the landing gate: ten suites **back to back**, same tree, loads 6.4–9.7 | 10 | **6** |

**The last row is the most useful thing here and it is a second measurement,
not more of the first.** Same tree, same session, same command — run with no gap
between suites so the host never settles, and the rate goes from about one in
five to six in ten. `71_macro_empty_arg` in the same ten reds once, so it is
this name that moves. A rate that tracks the host is what a bring-up race looks
like, and the harness agreed: every occurrence in that batch came back
`ALONE: GREEN`, which is its name for a test that fails only beside other
guests. Both rows are on `src/redlist.rs`, deliberately as two rows.

## The blame, and it named one aggressor when there were two

**The first four rows above were read as "the drain masks interrupts and
bounding it halves the rate", and that account was incomplete.** L3's review
found a second holder of the same lock with a worse shape: `write_console` took
`BackendGuard` — `cli` plus a global spinlock, with the device write inside it —
once for a **userland-chosen** length, because `SYS_WRITE`'s buffer has no cap
and the byte ring this branch deleted had never held that lock at all. So a
guest doing ordinary console output could mask interrupts for as long as it
liked, on the same machine whose i8042 was being brought up, and that window was
live for every measurement in the table's third and fourth rows.
`specs/log-architecture-spec.md` §8.1 carries the fix; the drain's eight-record
bound and this one are the two halves of what `kernel/CLAUDE.md`'s
`BackendGuard` caveat asks for.

**Bounding it does not move the rate, and that is what settles the blame.** The
last two rows are one session — an interleaved A/B of five suites per arm,
plus the four the landing gate then ran on the bounded arm — and the rate is
where it was: 2 of 9 against 1 of 5, which for counts this size is no movement
at all. The isolated re-run the harness takes afterwards did not even agree with
itself across the two occurrences: `red again` on one, `ALONE: GREEN` on the
other, which is what a race whose window is somebody else's looks like from
here. So what is left is not an interrupts-off window this branch owns; it is
the driver race the two halves below describe, which the branch's timing
exposes and does not cause. Recorded rather than re-run away.

`cargo run -- --known-red i8042_undecoded_bytes` said **NOT ON THE LIST** when
this entry was opened; it now answers the row that cites it —
`src/redlist.rs`, FIRES 3 of 14, dev host loaded, 2026-08-15.

## Two halves, and they want different fixes

- **The driver's**, and it is the real one: an interrupt whose byte the init
  path has already taken should not be counted as an undecodable byte. Either
  init masks the GSI while it polls, or the ISR's "no byte" case is
  distinguished from the "byte that decoded to nothing" case the report is
  about. The second is cheaper and is what the report's own wording already
  implies.
- **The gate's**: `i8042_undecoded_bytes` takes the first `nothing decoded` line
  in the capture and assumes it is the one its injection produced. Any earlier
  one — from boot, from a real spurious interrupt on a laptop — makes it read
  the wrong line. It wants the first line *after* its injection, which it knows
  the time of.

Neither is the log branch's to fix, and the entry says so rather than the branch
carrying a red it did not cause the shape of.

## 2026-08-16: the first CI sighting, and it is a *third* producer

PR #94 (`wt/toyos-schedfuture`, five documentation files), `ci` run
`31944633004`, job `95158684534` (`guest (2)`). Every row above is dev-host TCG;
this is KVM with one guest on the machine:

```
FAIL i8042_undecoded_bytes: the line names no byte: [kernel 2.494 cpu1] i8042: 1 interrupts and 4 bytes, nothing decoded — first seen at 2494ms
```

`ALONE i8042_undecoded_bytes: GREEN, and it was alone both times — nothing the
harness controls differed, so it failed once and passed once. That is a rate and
not a classification.` The green re-run's own lines, from the same job:

```
[kernel 2.816 cpu0] i8042: 2 interrupts and 6 bytes, nothing decoded — no event from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2816ms
[kernel 3.017 cpu0] i8042: the pin asserts — 4 interrupts, 8 bytes, 2 keys, 0 motion, no event from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2816ms
```

**Four bytes, not zero, and the stamp is the injection's own.** This is not the
bring-up line the heading names: Pause is six bytes, the red run reported after
the first interrupt had delivered four of them, and `Partial` holds a run until
the byte that *ends* it — so `UNEXPLAINED_N` was still zero and `Unexplained`
rendered nothing. The counters are honest and the conclusion is not: the
sequence had not finished arriving.

So the mute line has three producers that name no byte, and only two of them are
this entry's. The third is its own defect in its own file —
`specs/issues/kernel/the-i8042-mute-verdict-cannot-revise-a-line-it-said-too-early.md`
— because the fix differs in kind: no rule about *when* the report is triggered
reaches it, the driver has to be able to revise a verdict it has already said.

## What landed, 2026-08-16

Both halves this entry sanctions, and neither reaches that third producer:

- **The driver** (`kernel/src/drivers/i8042/mod.rs`). `EMPTY_IRQS` counts the
  interrupts the ISR found nothing behind, and "nothing decoded" is claimed only
  when something arrived to decode. The empty case gets its own sentence (`N
  interrupts and no byte behind any of them`), leaves the mute verdict owed, and
  rides the periodic counter line as `{} empty` for the ordinary case of one at
  bring-up followed by a keyboard that works. A second producer of the same
  false report went with it: `service` drains before it reports but the pin is
  live between the two, so a `has_bytes` load defers the report to the pass that
  holds the byte.
- **The gate** (`tests/toyos.rs`, `tests/common/serial.rs`). `must_say_after`
  reads the first line of a shape *after* a marker, and `i8042_undecoded_bytes`
  anchors both of its lines on `===I8042_READY===` — the marker its injection is
  timed off, and the only boundary the test knows without a host clock.
  `serial_vocabulary` gates the matcher against a constructed capture carrying
  this entry's own stranger line, in both directions: the whole-capture scan
  reads the stranger, the anchored one does not, and a capture with only the
  stranger in it has no answer at all.

**What is still open here is the race itself.** Its remaining consequence is not
this test: `i8042_health`'s quiet boot waits for `the pin has never asserted` as
its ready marker, and an empty interrupt during bring-up makes that line untrue
and so unsaid — the boot then fails on its marker rather than on a verdict.
Closing that is the other half of the driver's two options above — `init` masks
the GSI while it polls — and nothing has measured how often it bites.

## 2026-08-17: the driver half did not hold, and why

The retirement above was withdrawn the next day (#107). The driver half claimed
the report says `nothing decoded` only when something arrived to decode. **It did
not, and the counters said so without a boot**: the ISR added to `IRQS` on entry
and to `EMPTY_IRQS` only after the drain came back empty, with the whole
port-drain loop between them, while `report_health` computed
`carried = IRQS - EMPTY_IRQS` and printed whenever `carried > 0`. A reader
landing inside that window read `carried = 1` for an interrupt that carried
nothing. Counting an empty interrupt *apart* fixes nothing if the two counts are
still read at different instants.

Independently observed while that was being written: 2 of 6 full suites on
2026-08-17, `1 interrupts and 0 bytes … first seen at 449ms`, on a tree carrying
the fix.

### The withdrawal named the wrong producer, and this is the correction

The withdrawal went on to say the torn read "prints this row's line exactly".
**That clause is wrong**, corrected 2026-08-17 (PR #114) by the author of the
repair rather than by its author.

The torn read was real and is fixed. But it is not what printed
`[kernel 0.418 cpu1] i8042: 1 interrupts and 0 bytes, nothing decoded`, and the
boot order is what settles it: the reporting CPU is **`cpu1`, an AP**, while
`i8042::init` runs on the BSP *before* `smp::boot_aps` — so at the bring-up
interrupt this entry is named for, there is no second CPU in existence to land
inside the ISR's window. A reader has to exist before it can read anything torn.

What did print it is a different window in the same handler, and the section
below has it: `IRQS` was incremented on **entry**, ahead of the first
`push_isr`, so a reader between the pin asserting and the first byte reaching the
ring held a count of arrived bytes with no byte anywhere.

**The withdrawal itself stands, and that is the half that matters.** Retiring on
reasoning alone was wrong whichever mechanism the reasoning named — and the
correction is an instance of the same failure one level up: a torn read was
proved to exist and then asserted to be *the* producer of an observed line
without checking that a reader could have been there. Proving a race exists and
proving it is the one that fired are two claims, and the second needs its own
evidence.

**The lesson about the fix is about the shape of the claim, not this driver.**
"Counted apart" is a statement about a settled pair; the report is made against
an unsettled one. A fix that adds a second counter has to say at which instants
the two agree, and that one never did.

## What actually fixed it, 2026-08-17

`kernel/src/drivers/i8042/tally.rs`: the pair is **one `u64` the ISR writes once,
after the burst** — low half the interrupts that put a byte in the ring, high
half those that found none — and `Counts` can only be built by `Tally::read`,
which is one load. There is no subtraction left to be wrong and no instant at
which the halves disagree about the same interrupt. Narrowing the window was
never available as a fix, because the report is a statement about a completed
observation and the ISR had not finished making it.

Moving the write to the end of the ISR closed a second producer of the identical
line, and it had nothing to do with the empty case: `IRQS` moved on the way *in*,
so a reader between the pin asserting and the first `push_isr` held a count of
arrived bytes with no byte anywhere. That is what `1 interrupts and 0 bytes …
first seen at 418ms` on `cpu1` actually was — the reporting CPU is an AP, and
`i8042::init` runs on the BSP before `smp::boot_aps`, so no reader can be inside
the bring-up ISR at all. The empty interrupt is the *occasion* named by this
entry's heading; the entry-time increment is what let any first interrupt print
the line.

A third went with them, at the other end of the same byte's life: `RX_BYTES` was
added up after the drain loop, so a byte popped from the ring and still in front
of a decoder was in neither `has_bytes()` nor `RX_BYTES` for the length of its
own decode. `pop` counts it before releasing the slot, so the mute verdict's
`has_bytes` guard means what it says. All three are the same statement — *the
report never claims a verdict about bytes that did not arrive* — so they landed
together rather than as three entries.

Between them, `N interrupts and 0 bytes, nothing decoded` is unprintable:
`carried > 0` means bytes reached the ring, and a byte that reached the ring is
in it or in `RX_BYTES` at every instant. The one exception names itself, a ring
overflow, which `drain` logs on its own line.

**The gate is `kernel-loom/tests/i8042_tally.rs`, and it was measured in both
directions rather than argued.** With the two counters put back in `tally.rs`
both properties red — `Counts { carried: 1, empty: 0 }` for an interrupt that
carried nothing, and a count of an arrived byte with the byte unpublished — and
with the word restored all three models pass. A third model asserts the
transliterated old shape really *is* read torn, collected across loom's
executions, so the day loom stops exploring that window the file reds instead of
passing vacuously. The test half — anchoring on `===I8042_READY===` — was left
exactly as it landed; nothing found it wrong.

What that does **not** reach, and neither does anything here: the mute verdict
said too early about a sequence still arriving
(`specs/issues/kernel/the-i8042-mute-verdict-cannot-revise-a-line-it-said-too-early.md`),
which is the CI row under this test's name and a line with a non-zero byte count.
The `i8042_health` marker above is also untouched — an empty bring-up interrupt
still happens and still makes `the pin has never asserted` untrue — which is why
this entry stays open.
