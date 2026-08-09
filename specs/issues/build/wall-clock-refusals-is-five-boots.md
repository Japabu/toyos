---
status: open
kind: finding
opened: 2026-08-08
---

# `wall_clock_refusals` is five boots in one registration, and can be the longest job in the parallel phase

`tests/toyos.rs:465` registers it `Sched::Parallel`; the body
(`tests/common/wallclock.rs:281-305`) calls five helpers in sequence and each one
goes through `boot_and_read` (`:123-176`), which builds an image and boots one
guest. Five distinct kernel builds: `rtc-dead`, `rtc-unstable`,
`rtc-no-century`+`rtc-century-next`, `rtc-century-next`, `rtc-zone-east`. None is
in `INERT_ACTUATORS` (`tests/common/qemu.rs:1315-1322`), so `fold_inert` merges
nothing — one worker takes all five, serially.

Recorded durations on disk, from other worktrees' `target/test-durations`
(this one has never been run):

| file | entries | `wall_clock_refusals` | rank | that run's next-longest |
|---|---|---|---|---|
| `toyos-h3` (2026-08-08 00:08) | 285 | **209 405 ms** | **1st** | `i8042_kbd_echo` 187 280 |
| `toyos-hdaprobe` (2026-08-06 11:47) | 258 | **18 989 ms** | 8th | `xhci_msi_only` 39 054 |

The h3 figure is from a contended run (`parallel-tests-red-under-other-suites`'s
class), so "longest" is that run's verdict and not a clean measurement; 19.0 s
is the uncontended shape. Its
already-split sibling `wall_clock_file` costs 2818/3189 ms in the same two files.
`longest_first` (`tests/toyos.rs:9991`) can order jobs and can never split one,
so the ordering cannot help here.

**The split is free.** The kernel artifact memo (`src/build.rs:696-745`, keyed at
`:772` on `[PROFILE, features]`) builds one kernel per feature set per process,
so five registrations build exactly the five kernels the one registration already
builds — and the parallel phase gets five jobs it can place instead of one it
cannot.
