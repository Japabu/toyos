---
status: open
kind: defect
opened: 2026-08-17
---

# `screen_console_shell` reads a scrollback sitting at the head of the seeded log, and its failure message names the wrong cause

`screen_console_shell` waits for `CONSOLE_PROMPT` on the panel and then requires
an `i8042:` line above it, as the seed's witness: `seed_kernel_log` hands
`/boot/toyos/kernel.log` to the console in one `write_bytes` at startup, so a
panel with none of it is a console that started blank.

**On PR #111's `guest (3)` the panel was not blank — it was at the *top* of the
log**, and the message printed the one cause it cannot have been:

```
FAIL screen_console_shell: no `i8042:` line above the prompt:
  `/boot/toyos/kernel.log` never reached the scrollback, so this console starts
  blank where the diagnostic boot starts with the log
decoded screen:
2026-08-17 14:54:22 [0.000 cpu0 boot] panic console: armed 1920x1080 …
2026-08-17 14:54:22 [0.000 cpu0 boot] serial: 16550 loopback read 0xae (present)
…
2026-08-17 14:54:22 [0.000 cpu0] ioapic: iso bus:irq->gsi [0:0->2 edge/high, …
```

Every line on it is stamped `0.000` and comes from the first screenful of the
boot. `i8042: FADT rev …` is hundreds of lines later, so the assertion is really
"the panel is showing the *end* of the seed", and what it observed is a panel
showing the beginning of it. Either the seed had painted only its first
screenful when the prompt arrived, or the view was left at the head; the capture
cannot separate those and the test does not try.

Two things are owed and they are different sizes:

- **The message.** It states a cause it did not establish, which is the kind of
  line that sends the next reader after the wrong subsystem — `/boot/toyos/kernel.log`
  demonstrably reached the console here.
- **The race, if it is one.** `screendump_while` stops at the first frame
  carrying the prompt, so nothing makes the seed's paint and the prompt ordered.
  `specs/issues/hardware/collapsed-scroll-paint-unasserted.md` already records
  that no test asserts the panel after the seed; this is the same seam seen from
  the other side.

`ALONE: GREEN, and it was alone both times — nothing the harness controls
differed, so it failed once and passed once. That is a rate and not a
classification.`

Found on a PR whose diff is the i8042 interrupt tally, and ruled out as its
cause rather than assumed: that change adds no boot line and removes none — every
`i8042:` line in a quiet boot (`FADT rev`, `ok selftest`, `kbd … scanning on`,
the aux line) is written by `init` and untouched — so the set of lines this test
looks for is the same before and after it. `src/redlist.rs` carries the row.
