---
status: open
kind: defect
opened: 2026-08-08
---

# Four exception gates are installed with no test behind them, and each for a different reason

Closed by `wt/toyos-idt`: every vector Intel names for 64-bit mode now has a
gate, and `fault_gates` is the guest test. What that test cannot reach is worth
keeping, because it is the list a later change makes reachable.

Measured on the QEMU profile the suite boots, off `info registers` on a live
guest whose GDT limit says the kernel had already loaded its own:
`CR0=0x80010033`, `CR4=0x00310668`. Those are now `arch/control_regs.rs`'s
declaration, held by every CPU and asserted there, so each bit named below is a
decision rather than an observation.

- **#NM (7)** — CR0.TS and CR0.EM are both clear, so nothing can raise it. Only
  a lazy-FPU scheme would, and there is none.
- **#AC (17)** — CR0.AM is clear, so RFLAGS.AC buys a Ring 3 process nothing.
  The `ac` arm sets AC, reads it back as 1, and the misaligned load still does
  not fault.
- **#MC (18)** — CR4.MCE is in `CR4_REQUIRED`, so a machine check arrives on
  vector 18 rather than shutting the
  processor down. Nothing in the harness can stage one. Handled as an abort:
  `machine_check_handler` halts from either ring rather than killing a process
  over a machine that has stopped being trustworthy.
- **#XM (19)** — CR4.OSXMMEXCPT is set, so the architecture delivers this one;
  TCG does not. With the SSE invalid-operation exception unmasked, `0.0/0.0`
  leaves `MXCSR=0x00001f01` — IE raised, IM clear — and takes no trap. Real
  hardware would fault, so this arm is untested rather than unreachable.

Two more are kernel-only by construction and stay untested: **#TS (10)** needs
a task switch or an `iretq` to a bad TSS, and **#NP (11)** needs a descriptor
with `P = 0` inside the GDT limit, which this seven-entry GDT does not have.

And **#SS (12) is not reachable under TCG at all**: QEMU raises `EXCP0D_GPF`
for every non-canonical access and models #SS for none, so both `ss` arms — one
through RBP, one through RSP itself — come back as `SIGBUS … general protection
fault`. On metal the SDM gives #SS for an SS-relative non-canonical address, so
that gate is exercised by the same arms on hardware and by neither here. It is
the vector the AMD `SYSRET` residue in `issues/kernel/` would arrive on.
