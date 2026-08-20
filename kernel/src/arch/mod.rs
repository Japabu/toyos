pub mod apic;
pub mod control_regs;
pub mod cpu;
pub mod debug;
pub mod entry;
pub mod fpu;
pub mod idt;
pub mod mtrr;
pub mod pat;
pub mod percpu;
pub mod smp;
pub mod syscall;
pub mod tlb;

/// The witness that one log reservation and its publication cannot be
/// preempted on this CPU.
///
/// **Both IF and TF are clear.** IF excludes IRQ delivery and scheduler
/// preemption; TF matters independently because Ring 3 may set it and a #DB
/// handler logs before returning. Leaving TF set lets that handler reserve a
/// whole newer generation while the interrupted writer is halfway through its
/// slot body.
///
/// The bracket is deliberately narrower than formatting: it covers only the
/// shard pointer and identity reads, the unlocked `xadd`, and the body
/// publication — three identity words plus the message's own `ceil(len/8)`, at
/// most 1,016 bytes and in practice nine words. It takes no lock and performs
/// no locked read-modify-write.
#[must_use = "dropping the log commit guard reopens interrupts and single-step traps"]
pub(crate) struct LogCommitGuard {
    rflags: u64,
    /// Restoring saved RFLAGS is a same-CPU operation. Keep safe code from
    /// moving this guard to another CPU before Drop.
    _not_send_sync: core::marker::PhantomData<*mut ()>,
}

impl LogCommitGuard {
    pub fn close() -> Self {
        const TF: u64 = 1 << 8;
        const IF: u64 = 1 << 9;

        let rflags: u64;
        unsafe {
            // Deliberately no `nomem`: besides these instructions using the
            // stack, the implicit memory clobber keeps shard selection and
            // publication on the closed side of this compiler barrier.
            core::arch::asm!(
                "pushfq",
                "pop {saved}",
                saved = out(reg) rflags,
            );
            // **`log-unbracketed-reserve` is the negative control on this whole
            // type** (§9.4): the guard is constructed and dropped exactly as it
            // is now, and it masks nothing — so a producer can migrate between
            // reading its shard pointer and the `xadd`, and can resume its body
            // copy after a whole newer generation has committed into the same
            // slot. In the shipping kernel the accessor is `const fn … { false }`
            // and this folds to the unconditional `cli` it replaced.
            if crate::actuator::log_unbracketed_reserve() {
                return Self { rflags, _not_send_sync: core::marker::PhantomData };
            }
            core::arch::asm!("cli");
            if rflags & TF != 0 {
                let masked = rflags & !(TF | IF);
                core::arch::asm!(
                    "push {masked}",
                    "popfq",
                    masked = in(reg) masked,
                );
            }
        }
        Self { rflags, _not_send_sync: core::marker::PhantomData }
    }
}

impl Drop for LogCommitGuard {
    fn drop(&mut self) {
        unsafe {
            // Deliberately no `nomem`: the final slot store must stay before
            // interrupts and single-step traps are reopened.
            core::arch::asm!(
                "push {saved}",
                "popfq",
                saved = in(reg) self.rflags,
            );
        }
    }
}

/// Add one to a counter **only this CPU writes**, atomically against an
/// interrupt on it and against nothing else, and answer the value before the
/// add.
///
/// One `xadd` with **no `lock` prefix**. That is the whole point: a locked
/// read-modify-write is not one instruction under TCG — QEMU leaves the
/// translation block to run it exclusively — and one `fetch_add` per log line
/// cost 350 ms of boot
/// (`issues/hardware/one-rmw-per-log-line-cost-350ms.md`). An unlocked
/// `xadd` still retires whole, so an interrupt on this CPU cannot split it.
///
/// [`LogCommitGuard`] is the bracket. It lives at the call site because the
/// reservation and the body publication are one operation: reopening IF after
/// the `xadd` lets a preempted writer resume after a whole newer generation has
/// committed into the same slot.
///
/// # Safety
/// `counter` must be a word no other CPU ever writes, and `guard` must cover
/// the shard selection that established this CPU is its owner.
#[inline(always)]
pub unsafe fn percpu_fetch_add(
    counter: &core::sync::atomic::AtomicU64,
    _guard: &LogCommitGuard,
) -> u64 {
    // **`log-shared-reservation` is the negative control on the instruction
    // itself** (§9.4): a load, a window, and a store, which is the shape that
    // is *not* atomic against an interrupt on its own CPU. The window is what
    // makes it deterministic rather than a race — the defect being staged is
    // exactly "something came between the load and the store", and on one CPU
    // the only thing that can be made to come between them is an interrupt this
    // kernel sent itself. `log::nested`'s one-shot is consumed here instead of
    // mid-body, so the handler's first record takes the sequence number the
    // interrupted writer had already read. In a shipping kernel the accessor is
    // `const fn … { false }` and this whole branch folds away.
    if crate::actuator::log_shared_reservation() {
        let previous = counter.load(core::sync::atomic::Ordering::Relaxed);
        if crate::log::nested::inject() {
            unsafe {
                // The window, and nothing else in the machine opens one here.
                core::arch::asm!("sti");
                for _ in 0..256 {
                    core::hint::spin_loop();
                }
                core::arch::asm!("cli");
            }
        }
        counter.store(previous + 1, core::sync::atomic::Ordering::Relaxed);
        return previous;
    }

    let previous: u64;
    unsafe {
        // No `preserves_flags`: `xadd` changes arithmetic flags. The guard's
        // later `popfq` restores the caller's flags, but code between these two
        // asm blocks must still see an honest compiler contract.
        core::arch::asm!(
            "xadd [{ptr}], {out}",
            ptr = in(reg) counter.as_ptr(),
            out = inout(reg) 1u64 => previous,
        );
    }
    previous
}
