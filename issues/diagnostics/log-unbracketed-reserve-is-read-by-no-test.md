---
status: open
kind: defect
opened: 2026-08-22
---

# `log-unbracketed-reserve` is armed by nothing, so `LogCommitGuard`'s claim has no negative control

`kernel/src/actuator.rs` declares `log-unbracketed-reserve` as "the correctness
claim the whole design rests on, and the only thing that can make it fail on
purpose", and names its reader: "`log_migration_storm` at `--smp 8` is what
reads it."

**There is no `log_migration_storm`.** `rg log-unbracketed-reserve` over the tree
finds three sites — the declaration, the accessor's call in
`arch::LogCommitGuard::close`, and the doc comment there — and nothing under
`tests/`. No `BootOptions::kernel_params` in the harness names it.

Measured 2026-08-22 by arming it by hand, adding it beside `log-storm` in
`tests/common/logread.rs`'s `storm` (reverted afterwards) and running the one
name that could plausibly read it:

```
cargo test --test toyos-build -- log_conservation_smp8
  [log] smp=8: emitted=8192 read=4088 dropped=4104 concurrent=4032 lost=4294 wakes=2
  PASS  log_conservation_smp8  (3s)
```

green, four runs of four, with the bracket that stops a producer migrating
between its shard-pointer read and its unlocked `xadd` **removed**. The
conservation arithmetic still balances (4088 + 4104 = 8192) because what that
gate asserts — the shard count, that some records were read while the storm ran,
that some were read at all — is not what the bracket protects.

So the guard's `cli` is unmeasured: any change to it, including deleting a branch
from it, is landed on argument alone. That is what this change did today
(`arch::LogCommitGuard::close`'s `TF` branch), and the argument is sound because
the branch was unreachable — but the next change to that function has no gate
either.

What is owed is the reader the actuator names: a storm whose producers migrate
across CPUs, at `--smp 8`, asserting something the unbracketed kernel breaks —
a slot body that carries two generations, or two CPUs' `head` read-modify-writes
colliding. Until it exists the actuator is a declared instrument nothing has ever
run, which is the shape `issues/build/clippy-has-never-run-here.md` describes one
layer over.
