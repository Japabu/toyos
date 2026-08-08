---
status: open
kind: defect
opened: 2026-08-04
---

# A whole parallel phase can be starved by another agent's build

Measured 2026-08-04: the same tree that runs the phase in 44.8 s ran it in
245.2 s with three other `cargo test` processes and a `toyos-sched-sim measure`
on the host. Nothing in the suite reports this — the phase is simply slow, and
whichever load-sensitive `Sched::Parallel` test loses its margin first is what
goes red. `uptime` before and after a suspicious run, and `ps aux -r | head`,
are what separate it from a regression; a `toyos-tests-<pid>` directory per live
suite in `$TMPDIR` names how many are up.

`buildlock::guest_slot` bounds the *guests* to twelve across all worktrees, and
that is the part of this the semaphore closes. `buildlock::build_slot` (added
2026-08-07) bounds the compiles to four, which closes the part this entry is
actually about — "another agent's build" is a `cargo test`'s or a
`cargo run`'s, and both go through `src/build.rs`. What is left outside both
counts is work that reaches neither: a `toyos-sched-sim measure`, a `cargo build`
typed by hand in a fork clone, `./x.py` run directly in `rust/`. Each wait is
announced (`[host-slots] waiting …`, `[host-builds] waiting …`), which is what
separates a slow phase from a starved one without `ps`.
