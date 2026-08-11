pub mod apic;
pub mod control_regs;
pub mod cpu;
#[allow(dead_code)]
pub mod debug;
pub mod entry;
pub mod fpu;
pub mod gdt;
pub mod idt;
pub mod mtrr;
pub mod pat;
pub mod percpu;
pub mod smp;
pub mod syscall;
pub mod tlb;


/// Add one to a counter **only this CPU writes**, atomically against an
/// interrupt on it and against nothing else, and answer the value before the
/// add.
///
/// One `xadd` with **no `lock` prefix**. That is the whole point: a locked
/// read-modify-write is not one instruction under TCG — QEMU leaves the
/// translation block to run it exclusively — and one `fetch_add` per log line
/// cost 350 ms of boot
/// (`specs/issues/hardware/one-rmw-per-log-line-cost-350ms.md`). An unlocked
/// `xadd` still retires whole, so an interrupt on this CPU cannot split it.
///
/// The `cli` bracket is here rather than at the call site because the property
/// this function claims — "no other CPU writes this word" — is false the moment
/// the caller can be migrated. `pushfq`/`popfq` rather than `preempt::disable`,
/// which is itself two locked read-modify-writes.
///
/// # Safety
/// `counter` must be a word no other CPU ever writes, and the caller must have
/// established that this CPU is its owner.
#[inline(always)]
pub unsafe fn percpu_fetch_add(counter: &core::sync::atomic::AtomicU64) -> u64 {
    let previous: u64;
    unsafe {
        core::arch::asm!(
            "pushfq",
            "cli",
            "xadd [{ptr}], {out}",
            "popfq",
            ptr = in(reg) counter.as_ptr(),
            out = inout(reg) 1u64 => previous,
            options(preserves_flags),
        );
    }
    previous
}
