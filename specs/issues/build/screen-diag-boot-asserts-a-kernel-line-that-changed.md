---
status: open
kind: defect
opened: 2026-08-15
---

# `screen_diag_boot` asserts a kernel line the kernel stopped writing

The gate reds on both nightly dispatches of 2026-08-15 with

```
FAIL screen_diag_boot: "log: this boot is on the console and in" is not on
screen five seconds after the boot finished
```

and the decoded screen it prints beside that message carries

```
[0.476 cpu0] log: this boot is on the console and on /log
[0.477 cpu0] Boot: complete (477ms)
```

**The mode works; the string is stale.** `tests/toyos.rs` wants
`"log: this boot is on the console and in"`, which was the shape of the line
while the *kernel* opened the log file and could name it. It has not written
that since `9ca7631` ("`/bin/logd` writes the file, and the kernel stops being
a logger"), which cut the line to `log: this boot is on the console`, and
`ecede44` then restored the half the kernel still knows — whether the log
volume mounted — as `log: this boot is on the console and on /log`
(`kernel/src/main.rs`, `report_log_destination`). `ecede44` corrected
`screen_log_absent`, which reads the same table's alert arm, and left this one.

**Why a red on `main` survived a day of pull requests.** `screen_diag_boot` is
`Tier::Nightly`, so no pull-request gate runs it; the nightly's own alarm job
fires on `schedule` and not on `workflow_dispatch`, and both of these runs were
dispatches. Nothing was going to say so.

Two runs, minutes apart, on two trees whose difference cannot reach it:

| run | job | tree | verdict |
|---|---|---|---|
| `31900045901` | `95049265216` (`guest (12)`) | `main` at `e064a96` | red, then `ALONE screen_diag_boot: red again — the defect is real.` |
| `31900050723` | `95049280299` (`guest (12)`) | `wt/toyos-ciwall` | the same message, and the same `red again` alone |

The fix is one string, and the durable half is that there is nothing holding
the two ends together: the assertion is a hand-copied literal of a kernel
sentence, and `screen_log_absent`'s is another. A gate that reads a log line
should name the constant the kernel formats, the way
`tests/CLAUDE.md`'s `/bin/init` caveat already requires of predicates keyed on
a program's prefix.

One thing the same screen says in passing: it ends `[page 2/2]`, so on QEMU's
stdvga grid the diag log now overflows one page.
`specs/issues/hardware/kernel-log-unreadable-once-userland-owns-the-screen.md`
records that this test's footer branch has never executed — still true, because
the string check above returns first, but it is now one string away from
running.
