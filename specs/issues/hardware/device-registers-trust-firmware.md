---
status: open
kind: defect
opened: 2026-08-04
---

# Device registers still take firmware's word for being uncacheable

Every `map_mmio` outside the scanout passes `CachePolicy::DeferToMtrr`, which is
PAT entry 0 — WB, the entry that takes whatever the MTRR says for the range.
That is correct only while firmware covers the PCI hole with a UC range
register. Nothing checks that it does. A BAR in a range no MTRR covers, under a
`MTRRdefType` of WB, is a set of device registers the CPU is free to cache and
to reorder, and no symptom names the cause.

It survived review for years because there was no alternative: with the reset
PAT there is no way to *say* UC without PCD or PWT, and the kernel wrote no PAT
at all. That is no longer true — `arch::pat` owns the table, and a third
`CachePolicy` selecting a UC entry would make every BAR uncacheable whatever
firmware did, at no cost, since Table 11-7 gives UC for a UC PAT entry under
every MTRR type. It is not in this change because nothing has been observed to
be wrong: `mtrr::range_type` can answer per BAR and no boot has been asked.

The measurement that decides it: log `range_type` for each BAR `map_mmio` is
given, on the T14 and on QEMU, and see whether any comes back other than UC.
`specs/userspace-drivers-spec.md` §"It works because firmware's MTRRs make the
PCI hole uncacheable" and `specs/iommu-spec.md` both record the same
dependency; this is the entry that says the machinery to remove it now exists.
