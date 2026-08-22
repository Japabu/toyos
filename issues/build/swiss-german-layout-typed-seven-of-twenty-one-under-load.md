---
status: open
kind: finding
opened: 2026-08-22
---

# `swiss_german_layout` typed 7 of its 21 characters under a loaded dev host

One sighting, dev host, 2026-08-22, in a full 12-wide suite running as company
beside a second loop of `fault_gates` + `panic_recovery` in the same worktree:

```
FAIL swiss_german_layout: typed "zyüöäà@"
  want it to contain "zyüöäà@€[<>\\êÊÜ^é^q§"
  FAIL  swiss_german_layout  (11s)
...
  PASS  swiss_german_layout  (3s)
  ALONE swiss_german_layout: GREEN — it fails only beside other guests, so its
  Sched::Parallel is wrong. The run stays red on the classification.
```

11 s against 3 s alone. The prefix is intact and the tail is missing, so this is
a capture that ended early rather than a translation that went wrong — the seven
characters it did get are the seven the assertion's first seven ask for.

`cargo run -- --known-red swiss_german_layout` answers **NOT ON THE LIST**, which
is why this file exists: the next reader of this name gets a sighting instead of
nothing.

## What this is not evidence about

The kernel under it was not a tree anyone can check out — it carried
`wt/toyos-symbols`'s deliberate negative-control mutation, which is confined to
the crash report's symbol lookup (`process::with_current_symbols`) and cannot
reach the i8042, the keymap or the panel. So the *load* is the candidate and the
kernel is not, but neither is measured: one observation, no denominator.

And `tests/CLAUDE.md` is explicit that the harness's own `ALONE: GREEN` line is a
hypothesis and not a finding — a red whose mechanism turns out to be a race must
not be answered by re-classifying its `Sched`.

## What would settle it

A denominator on this instrument: loaded suites of an unmutated tree, counting
this name, with the boot width recorded per run. If it fires at a rate, it earns
a `src/redlist.rs` row on `Instrument::DevHostLoaded` and the question becomes
whether the missing tail is the guest's typing or the host's capture window.
