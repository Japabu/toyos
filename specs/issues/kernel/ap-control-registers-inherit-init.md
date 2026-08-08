---
status: open
kind: defect
opened: 2026-08-08
---

# Every AP runs with caching disabled, `CR0.NE` clear, `CR0.WP` clear and `CR4.MCE` clear

Found by `arch/fpu.rs::log_state`, the permanent per-CPU control-register line
`wt/toyos-fpu` added for `specs/user-machine-state.md` §7. Dev host, TCG,
`smp=2`, one boot, reproduced identically across four guests in the same run:

```
[kernel 0.000 cpu0] fpu: cpu0 cr0=0x80010033 cr4=0x310668 xsave=0 osxsave=0 ...
[kernel 0.221 cpu1] fpu: cpu1 cr0=0xe0000011 cr4=0x310620 xsave=0 osxsave=0 ...
```

The AP trampoline (`arch/smp.rs`) sets `CR0.PE`, `CR4.PAE`, `EFER.LME` and
`CR0.PG` and touches nothing else, so an AP reaches long mode holding the INIT
value of `CR0` with two bits or'd in. `0x60000010 | 1 | 0x80000000` is exactly
the `0xe0000011` measured. `init_ap` then adds `CR4.OSFXSR`, `OSXMMEXCPT`,
`SMEP`, `SMAP`, `FSGSBASE` and `PCIDE` and nothing to `CR0`. The BSP's values
are firmware's, inherited, and were never compared against the APs'.

| bit | BSP | AP | consequence |
|---|---|---|---|
| `CR0.CD` (30), `CR0.NW` (29) | 0 | **1** | **caching disabled on every CPU but cpu0** |
| `CR0.NE` (5) | 1 | **0** | an unmasked x87 exception goes to FERR#/IGNNE, not `#MF` |
| `CR0.WP` (16) | 1 | **0** | supervisor writes ignore the read-only bit |
| `CR0.MP` (1) | 1 | **0** | `WAIT`/`FWAIT` no longer trap on `TS` |
| `CR4.MCE` (6) | 1 | **0** | a machine check is a shutdown, not `#MC` |
| `CR4.DE` (3) | 1 | **0** | debug-register access semantics differ |

Four separate defects, one cause. Ranked by what they cost:

**`CD`/`NW` set is the expensive one.** An AP with caching disabled is running
uncached. That is a performance defect large enough to distort every A/B this
tree has ever taken on more than one CPU, and it is invisible to every test
because nothing measures a per-CPU rate. Not measured here — measuring it needs
the fix, and this entry is a report.

**`NE` clear explains `specs/issues/isolation/`'s unexplained survivor.** `fault_gates`' `mf` arm
killed its child 6 of 6 alone and survived once in a 12-wide suite, printing
status word `0xb881` — IE and ES set, on the `fnstsw` two instructions past the
`fwait` that should have trapped on exactly that `ES`. "The state was not lost;
the trap was." With `NE` clear the exception is signalled on the external FERR#
pin, which nothing in a modern machine is listening to, and a child scheduled
onto an AP rather than the BSP would see precisely that. This is a hypothesis
with a mechanism and a measurement behind it, not a proof: confirming it means
running that arm pinned to an AP.

**`WP` clear is an isolation weakening**, not a hole userland can reach on its
own — it means a kernel bug that writes through a read-only kernel mapping
succeeds on 7 of 8 CPUs and faults on cpu0. A bug that reproduces on one core in
eight is the worst kind to own.

**`CR4.MCE` clear** turns a machine check into a shutdown with no report, on
every CPU but the BSP, on the one machine (the T14) with no serial port.

The fix is one place — the APs' `CR0` and `CR4` should be built to a declared
value rather than inherited from INIT, and the BSP's should be checked against
the same declaration rather than trusted. Deliberately **not** done in
`wt/toyos-fpu`: that branch owns the ring-transition invariant, and changing
what `CR0.NE` means on 7 of 8 CPUs in the same commit would make its own
`fpu_isolation` gate unattributable.

`CR4.OSXSAVE` is clear on both CPUs here, so hypothesis 1 of
`user-machine-state.md` §7 — firmware leaving `OSXSAVE` set, cpu0 permitting AVX
and the APs `#UD`ing on it, a migrating thread faulting — **cannot be answered
on this host**. Its mechanism is confirmed (the two `CR4`s genuinely differ);
its instance needs the T14.
