---
status: open
kind: track
opened: 2026-08-03
---

# There is no wifi, and the top half of it can be built today with no hardware

Nothing 802.11 exists in the tree. The T14's radio is an Intel AX210 and the
plan for it is roughly 18,000 lines of transport and firmware op-mode with no
harness — the uncomfortable core, and nothing has reduced it.

**But the top half is independent of all of it and fully host-verifiable.** A
frame and element crate, a WPA supplicant crate (reusable by any future
supplicant), a station state machine, and the firmware TLV pipeline can all be
written today with no hardware and no IOMMU. They are blocked on sequencing
alone.

**The bottom half is blocked on the IOMMU**, which is at translation with one
identity domain for the whole machine: there are no per-device domains, no
invalidation, and no DMA-mapping syscall for userland at all. Handing a radio
its own DMA is exactly what that subsystem exists for.

**One diagnostic should run before any of the bottom half is costed**, because
it is the only thing that can invalidate the architecture: read the radio
function's isolation scope and reserved regions off the real machine. It is
about 200 lines on a boot the owner is already doing, and it has never been run.

Facts that cost real time to establish:

- The firmware pin is exact and single-valued — one API version, two files
  totalling **1,733,880 bytes** against a 35,651,584-byte boot partition.
- Licensing is settled: 238 of 285 upstream files are dual GPL/BSD (elect BSD);
  the 15 GPL-only files in the AX210 path are never read; the mac80211 and
  cfg80211 headers and the simulator are GPL-only and excluded.
- The declarative/imperative boundary is a measured cliff, not taste: the
  firmware API headers need **6** distinct Linux headers and the device configs
  **2**, against **44** for the op-mode and 75 for the subset as a whole.
- toyos-cc's packed-bitfield gap is no longer silent — it asserts by name rather
  than misaligning a firmware command. The residual is how many of the 635
  `__packed` uses carry bitfields
  (`issues/build/toyos-cc-no-packed-bitfields.md`).
