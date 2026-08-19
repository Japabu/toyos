---
status: open
kind: defect
opened: 2026-08-15
---

# `MAX_CPUS` is defined three times and nothing pins the copies to each other

```
$ git grep -n 'const MAX_CPUS' HEAD -- kernel/src
HEAD:kernel/src/sched/mod.rs:24:pub const MAX_CPUS: usize = 8;
HEAD:kernel/src/shootdown.rs:34:pub const MAX_CPUS: usize = 8;
HEAD:kernel/src/trace.rs:55:pub const MAX_CPUS: usize = 8;
```

`sched/mod.rs:24` is the real one. `shootdown.rs:34` is justified and says so:
that file is compiled a second time by `kernel-loom`, which shims the crate away,
so it may not name `crate::sched`.

`trace.rs:55` has no such excuse — `rg -n 'crate::' kernel/src/trace.rs` returns
four hits, so the file is not loom-compiled and can name the real constant.

`rg -n 'assert.*MAX_CPUS' kernel/src` finds two asserts, and neither compares the
copies. So the three are held in agreement by nothing but the fact that nobody
has changed one.

## What breaks, quietly

Raise `sched::MAX_CPUS` to 16 for a machine with more cores and `trace.rs`'s
per-CPU array stays at 8. `trace.rs:242` then indexes it by CPU id and every
trace event from CPUs 8 through 15 is silently dropped — on the instrument whose
whole job is to say what the machine did. Nothing reds; the traces just get less
complete on exactly the machines big enough to need them.

The same shape applies to `shootdown.rs`'s copy, and there the failure is worse
than lost diagnostics — but that one is deliberate and its reason is written
down, so it needs the assert rather than the deletion.

## What a fix owes

Delete `trace.rs:55` and use `crate::sched::MAX_CPUS`. Give `shootdown.rs:34` a
`const _: () = assert!(MAX_CPUS == crate::sched::MAX_CPUS);` — under whatever
`cfg` keeps it out of the loom build, which is the only reason the duplicate
exists.

That is one line deleted and one added, and it converts a silent drift into a
compile error. Found during the 2026-08-15 mechanism-consolidation audit while
inventorying limit constants; verified at `71a0559`.
