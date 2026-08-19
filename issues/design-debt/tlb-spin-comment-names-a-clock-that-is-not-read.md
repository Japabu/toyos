---
status: open
kind: defect
opened: 2026-08-15
---

# `arch/tlb.rs`'s spin constant is defended by a claim about a clock the kernel never reads

```rust
// kernel/src/arch/tlb.rs:83
/// Spins between deadline checks. `nanos_since_boot` is an HPET read on the
/// ...
const SPINS_PER_DEADLINE_CHECK: u32 = 1024;   // :86
```

It never is. `clock::nanos_since_boot` (`kernel/src/clock.rs:99`) is
unconditionally three instructions' worth of TSC arithmetic:

```rust
pub fn nanos_since_boot() -> u64 {
    let delta = cpu::rdtsc().saturating_sub(TSC_BOOT.load(Relaxed));
    let period_fs = TSC_PERIOD_FS.load(Relaxed);
    ((delta as u128 * period_fs as u128) / 1_000_000) as u64
}
```

The HPET is read in `clock::init` to calibrate the TSC and nowhere else on any
machine. There is no fallback path and no "machines that have no invariant TSC"
branch — `issues/kernel/ap-tsc-trail-is-assumed-and-never-checked.md` is
the entry for the fact that nothing even measures the assumption.

A comment's stated reason is a claim too, separable from the rule it defends:
the rule can be right while the reason given for it is false. 1024 spins
between deadline checks may well be the right number. The reason given for it
is not a fact about this kernel.

There *is* a real cost that would justify a batching constant, and it is written
down 15 lines below the false one — `clock.rs:108-113` records that
`nanos_since_boot`'s 128-bit divide is `__udivti3`, an out-of-line call in
`compiler_builtins`, and that a spin reading the clock every iteration spends a
large fraction of its time outside the spinning code. That is the argument
`SPINS_PER_DEADLINE_CHECK` should have been given.

## What a fix owes

Replace the reason, not the constant — and only after checking whether the
`__udivti3` argument actually holds at 1024, since the number was chosen for a
different and untrue premise and nothing has measured it against the real one.

Found during the 2026-08-15 mechanism-consolidation audit while inventorying time
sources; verified at `71a0559`.
