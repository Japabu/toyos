---
status: open
kind: finding
opened: 2026-08-09
---

# What an AP's disabled caching cost is unmeasured, and only bare metal can measure it

`arch/control_regs.rs` closed the defect this slug is named for: until
2026-08-08 an AP kept the `INIT` value of `CR0` that the trampoline OR'd `PE`
and `PG` into, so cores 1..N ran with `CD`/`NW` set — caching off — and with
`WP`, `NE` and `MP` clear. Both registers are now written whole from one
declaration, applied on every CPU and asserted there, and the `control_regs`
gate holds every bit of `CR0` and eight of `CR4` against that declaration
independently of the kernel's own self-check.

What that leaves is a number nobody has: **how much an uncached AP cost.** It is
worth having because every multi-CPU measurement in this tree predates the fix
— the audio numbers, the scheduler work, the boot timings, and
`specs/user-machine-state.md` §12's `+123 cycles` per syscall, whose two arms
were both taken on a machine where three of four cores were uncached. None of
them is wrong; each is a measurement of a different machine, and without this
number there is no way to say how different.

**Neither instrument this project runs on can answer it.** Both rows are from
`6e2dac6`:

| host | an AP that has executed nothing but the trampoline |
|---|---|
| dev host, TCG | `cr0=0xe0000011 cr4=0x00000020` |
| CI shard 3, KVM on an Intel Xeon 6973P-C | `cr0=0x80000011 cr4=0x00000020` |

Under KVM the AP arrives with `CD` and `NW` **already clear**, so no CI shard
could ever have failed on the caching half of the defect however long it stood.
And TCG models no cache, so the bit is architectural state there with no timing
consequence — which the instrument itself measured, `smp=4`, 4096 cache lines
read either side of the `mov cr0` that turns caching on:

| | pre | cold | warm |
|---|---|---|---|
| cpu0, caching on throughout (the control) | 26000 | 22000 | 19000 |
| cpu1, `pre` uncached, `cold`/`warm` cached | 30000 | 27000 | 20000 |
| cpu1 under `no-ap-control-regs`, all three uncached | 30000 | 22000 | 20000 |

The third row is the verdict: leaving cpu1's caches off for all three passes
costs it nothing against the row where two of them ran with caches on. Every
number is a multiple of 1000 — TCG's TSC granularity — and cpu2 and cpu3 on the
same boot answered `11000/10000/5000` with byte-identical registers, a fourfold
spread that is the host's scheduling rather than the guest's.

**The instrument is built and has never been run on silicon.**
`cargo run -- --diag-boot --kernel-feature control-regs-bench --build-only`,
flash, and read the per-CPU rows off the panel; cpu0's row is the control,
because it arrives with caching already on, and every AP's `pre` against its own
`warm` is the number. Nothing shorter reaches it: there is no CPU affinity, so
no userland loop can choose the core it runs on, and the state under test exists
only between an AP's `INIT` and its first `mov cr0` — a window with no userland
in it at all. `syscall_cost` cannot be pinned to an AP for the same reason, and
on a host that prices `CD` at zero it would answer the same on both.

## What is *not* open, recorded because three places used to say it was

**`CR4.OSXSAVE` diverging between the BSP and the APs** —
`specs/user-machine-state.md` §7's hypothesis 1, firmware leaving it set so cpu0
permits AVX and an AP `#UD`s on it, killing a thread that migrates. The
declaration is written whole rather than OR'd, so `write_cr4` clears the bit on
the BSP as well; it is in neither `CR4_REQUIRED` nor `CR4_OPTIONAL`, the gate
asserts it clear on every CPU by name, and a second arm refuses any bit the gate
never named. The divergence is unrepresentable on the T14 as much as here, so
that hypothesis needs no machine.

**Whether `CR0.NE` clear was *the* cause of `fault_gates`' one `0xb881`
survivor** — an unmasked x87 exception signalled on FERR# instead of raising
`#MF`, on a child that happened to land on an AP. It was left open for want of a
way to pin the arm to an AP. It is no longer owed: `NE` is declared set and
asserted on every CPU, the `mf` arm asserts a kill rather than tolerating a
survivor, and a recurrence is a red naming the test instead of a line in a log.
