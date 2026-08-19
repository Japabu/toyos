---
status: open
kind: finding
opened: 2026-08-17
---

# The DTV's capacity is a fixed bound over a quantity the workload sets, and its refusal is unrecoverable

`kernel/src/loader/tls.rs:16` fixes `DTV_INITIAL_CAPACITY = 64`, and
`tls_alloc_block` (`arch/syscall.rs:2724`) refuses a `module_id` above it
with `ResourceExhausted`, deliberately: the DTV is a fixed-capacity array the
kernel wrote, so there is nowhere to record the answer. `abuse_tls_alloc` gates
the refusal.

The quantity bounded is *the number of TLS-carrying modules a process loads*,
which is set by the workload and by nothing else. This tree has just spent three
scheduler designs establishing that a bound over a workload-set quantity is a
defect rather than a policy, and this is one.

**The refusal has no recoverable form.** The only caller is std's
`__tls_get_addr_slow` (`rust/library/std/src/sys/pal/toyos/tls.rs`), and its own
comment says why it cannot pass the error on: *"`__tls_get_addr`'s ABI is an
address and there is nobody to return an error to: a refusal added to `offset` is
a pointer near the top of the address space that the caller would then
dereference."* It answers `rtabort!("no TLS block for a dlopen'd module")`. So
the 65th TLS-carrying module in a process is a process abort at an arbitrary
point in its execution, not an error a `dlopen` caller could handle.

**How close the tree already is.** Nothing shipped reaches it: measured with
`toyos-elf`, all 20 programs in `system.toml` have zero `DT_NEEDED` and zero TLS
relocations, so every shipped process has exactly one TLS module. The pressure
comes from the hosted rustc, which loads proc-macro dylibs with `dlopen`:
`ls rust/build/x86_64-unknown-toyos/stage2/lib/*.so | wc -l` → 18, and each
proc-macro one measured (`libserde_derive`, `libdarling_macro`) carries 165
`DTPMOD64` + 165 `DTPOFF64` relocations — i.e. it is a TLS module. A crate graph
with more than ~62 proc-macro dependencies aborts rustc.

Not fixed here. A growable DTV is a change to a structure `__tls_get_addr`'s
naked fast path indexes directly, and the honest fix may be that the kernel
should not own the DTV at all: moving the loader out to Ring 3 puts the DTV in
the process's own address space, where that address space is the bound. This
came out of the 2026-08-17 scoping of exactly that move.
