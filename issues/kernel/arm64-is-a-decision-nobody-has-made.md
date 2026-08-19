---
status: open
kind: track
opened: 2026-07-28
---

# ARM64 is a roadmap target with no code and no decision behind it

Nothing aarch64 exists: no `kernel/src/arch/aarch64/`, no target spec, no `cfg`
in `kernel/src/arch/mod.rs`, and `src/build.rs` hardcodes three x86-64 triples.
The port was researched on 2026-07-28 and never started.

**The blocker is a ruling, not code.** Without a commitment the remaining
justification for making `arch/` honest is one MMIO memory-type bug, two closed
issues and ~290 deleted lines — not worth a parallel world. With one, the kernel
grows an estimated **+2,300 to +2,800 lines** against 23,725, ~290 leave x86
permanently, and ~1,520 stop pretending to be architecture code.

What is unblocked and cheap regardless: the build system's three hardcoded
triples become an `Arch`, and three measurements turn guesses into facts before
any interface depends on them.

Constraints that cost real time to re-derive, all measured on an M4 Pro host:

- `qemu-system-aarch64` has **hvf**; `qemu-system-x86_64` on this host has only
  tcg. ARM64/HVF against x86-64/TCG: **6.5×** on an address-space switch
  (251 ns vs 1619 ns), 3.4× random RMW, ~1.4× atomics at smp≥2, 1.06× on an ALU
  loop — and a **5.2× regression** on MMIO (876 ns vs 168 ns per device-register
  access). The distortion is non-uniform, so no correction factor exists.
- A real boot is **4.96 s of host wall clock**, not the 387 ms the guest
  reports: 2.48 s initrd load, 1.42 s OVMF, 0.93 s kernel. Half of that is
  architecture-independent and fixable today.
- `ID_AA64MMFR0_EL1 = 0x000010000F100022` on this M4: TGran4 and TGran16
  supported, **TGran64 not**. Keep "2 MiB only" and use the 4 KiB granule with
  L2 block descriptors — `mm/paging.rs`'s 39/30/21 shifts are already
  bit-identical to ARM64 L0/L1/L2. Do not fix TLS fragmentation by changing the
  page size; suballocate.
- HVF takes away `-d int` (1,544 bytes of log against 126,975 under TCG), the
  guest PMU (`PMUVer = 0`) and EL2. `ID_AA64PFR0_EL1.GIC = 0` whatever
  `gic-version` says, and `ID_AA64ISAR0_EL1.RNDR = 0` — **no hardware RNG at
  all**, so `sys_random` would need an entropy pool that does not exist. Guest
  PA space is 40 bits. gdb keeps full parity.
- Dispatch was ruled on 2026-07-28: compile-time, statically resolved — one
  `cfg_attr(path)` module selection plus typed `const _: fn(…)` contract
  bindings. **No `dyn Arch`, no `Kernel<A: Arch>`.** 43 `crate::arch::`
  references resolve to ~20 distinct symbols, dominated by per-CPU identity
  queries on the lock and fault paths.

`issues/kernel/all-cores-are-assumed-equal-and-arm64-breaks-that.md` is the one
piece of this the owner has since queued.
