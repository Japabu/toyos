---
status: open
kind: defect
opened: 2026-08-19
---

# Every TLB invalidation is chosen by the caller instead of derived from what changed, and two of the choices are wrong

**The TLB is a closed problem and this tree does not treat it as one.** Unlike
placement or fairness, invalidation has no policy in it: a page-table mutation
falls into a small enumerable set, and SDM Vol. 3A §4.10.4 states exactly what
each one requires. There is a right answer per case, it is knowable without
measurement, and nothing in the kernel writes it down. Today the invalidation is
picked at each call site by whoever wrote the site, and two sites picked wrong.

This issue is about making the invalidation **a consequence of the mutation**.
The performance is a by-product; the defect is that a wrong invalidation is
currently expressible, and two of them are in the tree.

## What each case does today

Read out of the tree on 2026-08-19 at `8e9f851`. Every row was checked in the
source, not inferred.

| mutation | site | today | required |
|---|---|---|---|
| install where nothing was present | `mm/paging.rs:353` via `set_pde` `:114` | `invlpg` | **nothing** — no translation or paging-structure entry is created from a non-present entry |
| replace a present leaf, same address space | `mm/paging.rs:392` via `set_pde` | `invlpg` on current CR3's PCID | that, and it is correct |
| replace a present leaf, **another** address space | `loader/mod.rs:808` | `invlpg` on the **caller's** PCID | invalidate nothing here; the child has no entries — see defect 2 |
| clear a leaf, then free the page | `mm/paging.rs:409` + `Unmapped::drop` | `invlpg`, then machine-wide shootdown, then free | the ordering is right; the scope is wider than the SDM requires |
| direct-map leaf replaced with a different memory type | `mm/paging.rs:769` | machine-wide shootdown | **correct, do not narrow** — a stale WB entry against a live WC mapping is SDM §11.12.4 undefined |
| split a direct-map leaf for a guard page | `mm/paging.rs:669` | local full flush | correct, and reasoned at the site |
| context switch | `mm/paging.rs:200` | `CR3` with `NOFLUSH` | correct |
| PCID wrap | `mm/paging.rs:249` | machine-wide flush of every PCID | correct for a recycled tag; the recycling itself is a separate open defect |

## The two that are wrong

**1. `process.rs:1638-1639` invalidates the same address twice.**

```rust
addr_space.lock().remap(UserAddr::new(region_start), page_alloc.phys(), writable);
crate::mm::paging::invlpg(region_start);
```

`remap` reaches `set_pde`, which already calls `invlpg(va)` (`mm/paging.rs:116`).
The second call is the demand-paging fault path — the most frequently executed
paging path in the kernel — paying a `CR3` read and an `INVPCID` it just paid.
Neither line is wrong on its own, which is the point: the site cannot see that
the method it called already did this.

**2. `loader/mod.rs:808` invalidates the wrong address space.**

The `remap` writes into the **child's** page tables while executing on the
**parent's** CPU. `paging::invlpg` (`mm/paging.rs:177-183`) reads the *current*
`CR3` for its PCID:

```rust
let pcid = crate::arch::cpu::read_cr3() & 0xFFF;
crate::arch::cpu::invpcid(0, pcid, addr);
```

So this evicts the **parent's** live translation at the child's virtual address,
and does nothing at all for the child — which needs nothing, having never run.
Not merely wasted: it discards a good entry belonging to an unrelated process.

`arch/tlb.rs:94-98` already documents this exact hazard for the *unmap* side and
fixes it there by flushing the whole local TLB. The map side kept the bug. One
module knew and the neighbouring one did not — which is the shape this issue is
about.

## The shape that makes a wrong choice unwriteable

`PageTablePage::set_pde` (`mm/paging.rs:114`) is one method for two different
mutations. It cannot distinguish "install where the PDE was not present" from
"replace a present one", so it invalidates unconditionally — the only choice
that is never *unsafe*, and wrong for half its callers. `map_range` asserts
`existing & PAGE_PRESENT == 0` immediately before calling it (`:349-352`): the
caller has already proven the case, and then throws the proof away.

Make the two mutations two operations that cannot be confused — an install that
takes the proven-absent slot and **has no invalidation in it**, and a replace
that requires the address space it is replacing in. Then defect 1 is a duplicate
that does not typecheck, and defect 2 cannot name the caller's PCID because the
operation takes the target address space rather than reading `CR3`.

The usual bar applies: `mm/paging.rs` is `AddressSpace`'s own module, so
`set_pde`'s privacy is not the barrier — the distinction has to be in the types,
not in a comment saying which to call.

## Also unmade: there is no `PAGE_GLOBAL` anywhere

`CR4.PGE` is absent from `arch/control_regs.rs`'s declaration and no
`PAGE_GLOBAL` exists in `kernel/src/mm/`. With PCID active, every address space
therefore caches a private copy of the kernel's direct map, and every
`flush_tlb_all` — which is `INVPCID` all-context — throws away the kernel's own
translations on every CPU along with the process's.

This is not an oversight to fix silently: it is a decision nobody has made.
2020+ hardware is not Meltdown-affected and this kernel has no KPTI, so global
kernel pages are available. `control_regs.rs`'s own doctrine — every bit of both
registers is decided in one place, and the bits left out are as much of the
declaration as the bits in — means `PGE`'s absence should be *stated* whichever
way it goes.

## Deliberately not in scope

**Per-address-space CPU residency masks**, so an unmap interrupts only the CPUs
that ran the process. It is the obvious next idea and it is the wrong next move:
it is a new lock-free mechanism — a CPU adopting an address space races an
unmapper reading the mask — which in this tree means a `kernel-loom` model and
its negative controls before it may be trusted.

It also does not touch what makes `Unmapped::drop` expensive. That cost is not
*which* entries are invalidated: it is that `arch::tlb::shootdown` sends an IPI
to every CPU and spins with `IF` clear until all of them acknowledge, under a
5 s panic deadline. Narrowing the entry set leaves the IPI and the wait exactly
where they were. The lever for that path is not waiting at all — deferred
reclamation — which is the completion track's "a CPU never waits for a device"
(`issues/kernel/every-wait-in-this-kernel-is-a-spin.md`) applied to memory.

**Nothing in the kernel counts shootdowns**, so every quantitative claim about
this subject, including in this file, is unmeasured. The two defects above and
the `PGE` question are settled by reading the SDM and the source and need no
measurement; anything past them does.
