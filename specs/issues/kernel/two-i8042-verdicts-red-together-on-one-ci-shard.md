---
status: open
kind: defect
opened: 2026-08-19
---

# `i8042_keyboard` and `i8042_no_spurious_wake` went red together in one CI shard

**First sighting for both names.** `cargo run -- --known-red` answered
`NOT ON THE LIST` for each before this. Run `32249152467`, job `guest (2)`,
shard 2/12, `--jobs 1 --host-slots 0`, so the phase log reads
`--- parallel, 1 wide ---`: one guest on the machine at a time, KVM, and no
host contention to appeal to. Ten guests over the shard, three of them the
i8042 group.

This entry **records** the run. It does not name a cause, and where it quotes
the driver it says so as a quotation.

## What each one said

`i8042_keyboard`, 7 s:

```
no event for HID usage 0x29 in [KeyLine { usage: 11, modifiers: 0, translated: "h" }, …]
```

Twenty `KeyLine`s, ten press/release pairs. The scripted sequence is otherwise
all there and translating: `h e l l o`, then shift-`B` (`usage: 225` press,
`usage: 5` with `modifiers: 1` translating `"B"`, both released), then
`usage: 80` translating `"\u{1b}[D"`, then `usage: 77` translating
`"\u{1b}[F"`. HID usage `0x29` is Escape and appears nowhere.

One thing in the capture is worth writing down without being read as a cause:
**the second `usage: 225` press/release pair encloses no key event at all**,
where the first `usage: 225` pair encloses the `B`. So the capture is not
twenty lines of a nineteen-line script — a shift was pressed and released
around nothing.

`i8042_no_spurious_wake`, 6 s:

```
no drain produced zero events — the stimulus never landed:
```

## The second sentence is contradicted by its own capture

The capture that failure prints is one boot's, and it shows the stimulus
landing. Verbatim:

```
[kernel 2.679 cpu1] spawn: /bin/test_rs_i8042_keyboard pid=5 tid=0 …
===I8042_READY===
kev usage=0x04 mods=0x00 tr="a"
[kernel 2.840 cpu0] i8042: drain bytes=8 keys=2 motion=0 woke_kb=1 woke_ms=0
[kernel 2.840 cpu0] i8042: the pin asserts — 2 interrupts, 8 bytes, 2 keys,
    0 motion, no event from [0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5], first seen at 2839ms
kev usage=0x04 mods=0x10 tr=""
kev usage=0x04 mods=0x00 tr="a"
[kernel 3.031 cpu0] i8042: drain bytes=12 keys=4 motion=0 woke_kb=1 woke_ms=0
kev usage=0x04 mods=0x10 tr=""
kev usage=0x4d mods=0x00 tr="\u{1b}[F"
kev usage=0x4d mods=0x10 tr=""
kev done seen=6
```

`[0xe1, 0x1d, 0x45, 0xe1, 0x9d, 0xc5]` is the test's own Pause — the same six
bytes `src/redlist.rs`'s `i8042_undecoded_bytes` row already quotes. The kernel
names all six and reports no event from them, so **the stimulus did land**.
What did not happen is a drain carrying *only* it: the drain that took it
reports `bytes=8 keys=2`, six Pause bytes and two key bytes together, and the
next reports `bytes=12 keys=4`. Neither drain in the capture has zero events,
which is exactly the first clause of the failure and the opposite of the
second.

Alone, the same test reports `2 zero-event drains, none woke; 3 real ones, all
did` — so the shape it wants does normally occur.

## A third boot in the same phase, which passed

`i8042_mouse` passed in that phase and in both re-runs, on its motion count,
and its tally line differs between them:

| boot | interrupts | bytes | keys | motion | undecoded | empty |
|---|---|---|---|---|---|---|
| the failing phase | 1890 | 3021 | **0** | 1007 | **0** | 945 |
| re-run 1 | 1912 | 3067 | 28 | 1007 | 12 | 955 |
| re-run 2 | 1915 | 3067 | 28 | 1007 | 12 | 956 |

Zero keys and zero undecoded bytes in the phase where the two keyboard verdicts
went red, twenty-eight and twelve in both boots where they passed. The motion
count is identical in all three. **These are three separate boots** — each
capture starts its own kernel clock — so this is not evidence that one guest's
delivery went quiet for all three names; it is one more boot in the same phase
whose keyboard half decoded nothing.

## The re-runs

Each name was re-run as its group and passed **twice**, once in each group
retry: `PASS i8042_keyboard (5s)` and `(6s)`, `PASS i8042_no_spurious_wake
(227ms)` and `(222ms)`. The harness's own verdict for both:

```
ALONE …: GREEN, and it was alone both times — nothing the harness controls
differed, so it failed once and passed once. That is a rate and not a
classification.
```

Which is the whole of what is known: a rate, sample size one, on the instrument
that has no contention to blame.

## What to do

Nothing here is diagnosed and nothing should be re-run away. The two threads
worth pulling, in order of what the log already supports:

- **`i8042_no_spurious_wake`'s precondition is not something the test
  arranges.** It needs a drain that carries its Pause and nothing else, and
  what a drain carries is whatever the ISR found in the buffer. If a real key
  byte can share that drain — and here one did — the test is asserting on a
  batching it does not control. That is a defect in the instrument or in the
  batching, and which one it is decides whether the fix is in
  `tests/toyos.rs` or in `kernel/src/drivers/i8042/`.
- **`i8042_keyboard`'s missing `0x29`** is unexplained. The shift pair around
  nothing is the only structural oddity in the capture and may or may not be
  the same event.

`specs/issues/build/i8042-keyboard-pays-a-lost-sentinel-and-reds-the-durations-gate.md`
is a different failure of the same binary — a `durations` red, not a lost
event — and is not merged with this one.
