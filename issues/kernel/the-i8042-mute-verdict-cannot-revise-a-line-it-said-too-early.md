---
status: open
kind: defect
opened: 2026-08-16
---

# The i8042 mute verdict names no byte when it beats the sequence, and cannot revise itself

```
FAIL i8042_undecoded_bytes: the line names no byte:
  [kernel 2.494 cpu1] i8042: 1 interrupts and 4 bytes, nothing decoded — first seen at 2494ms
```

PR #94 (`wt/toyos-schedfuture`, five documentation files), `ci` run
`31944633004`, job `95158684534` (`guest (2)`), 2026-08-16 — the first CI
sighting of this name, and `ALONE: GREEN`. The isolated re-run in the same job
is what the line should look like:

```
[kernel 2.816 cpu0] i8042: 2 interrupts and 6 bytes, nothing decoded — no event from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2816ms
[kernel 3.017 cpu0] i8042: the pin asserts — 4 interrupts, 8 bytes, 2 keys, 0 motion, no event from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2816ms
```

## What it is

Pause is `E1 1D 45 E1 9D C5`, six bytes, and `i8042_undecoded_bytes` injects it
because it is the one key whose whole sequence decodes to nothing by design. The
red run reported after the first interrupt had delivered **four** of the six.

`Partial` (`kernel/src/drivers/i8042/mod.rs`) holds a decoder's run until the
byte that *ends* it, precisely so that a whole undecodable sequence can be named
rather than its last byte — so with two bytes still to come, `UNEXPLAINED_N` was
zero and `Unexplained` rendered nothing. The counters are honest and the
conclusion is not: nothing had decoded to nothing yet, the sequence had not
finished arriving.

**And the driver never comes back to it.** `report_health` says the mute line
once: `HEALTH_MUTE_SAID` is terminal for that branch (`next == state` returns),
and the only line after it is `the pin asserts`, which the typed letter
produces. So on this boot the bytes *are* named — on the wrong line for a test
whose whole subject is that the mute verdict names the byte.

This is deliberate as far as it goes, and the module says why: the first
interrupt's own pass is the earliest moment the verdict can be given and the
least settled one, and a report that went straight to `HEALTH_DONE` would freeze
the panel on a half-arrived sequence. The half that is missing is the revision.

## Not the entry beside it

`issues/kernel/an-i8042-interrupt-arrives-with-no-byte-during-init.md` is
the same *symptom* — a `nothing decoded` line naming no byte, which
`i8042_undecoded_bytes` reads as its own — from two other producers, and both
of its halves landed on 2026-08-16 without reaching this one:

- the line here is **after** the injection, so anchoring the gate on
  `===I8042_READY===` does not separate it;
- the interrupt here **carried bytes**, so counting the empty ones apart does
  not suppress it.

## What would close it

Two candidates, and neither is taken here because both are policy about what the
kernel's log says:

- **Revise once.** Split `HEALTH_MUTE_SAID` into "said, naming nothing" and
  "said, naming bytes", and allow the second line when `UNEXPLAINED_N` first
  goes above zero. At most one extra line per boot, and the module's own
  doctrine argues for it: `N bytes, 0 keys` is "a true statement that names no
  suspect", which is what `Unexplained` exists to replace.
- **Defer while a run is open.** Do not claim the mute verdict while a decoder
  partial is non-empty. Cheaper, and it has a hole the first does not: a mouse
  partial the framer abandons on the idle gap stays until the next run ends, so
  a machine that goes quiet mid-packet would say nothing at all — on the T14,
  whose only channel is the panel, that is the case the line exists for.

`i8042_undecoded_bytes` is `Tier::Fast` and runs on every pull request, so
whatever the rate is, it is being paid there.
