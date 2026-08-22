#![warn(clippy::undocumented_unsafe_blocks)]
//! The machine, and the only part of this kernel that knows which one.
//!
//! Every `unsafe` block under here carries a `SAFETY:` comment saying why it is
//! irreducible as well as why it is sound, and the attribute above is what keeps
//! that true — it composes with the `-D warnings` both kernel clippy invocations
//! already carry, so a new block without one does not build. It is also the
//! last of these a kernel area needed: `main.rs`'s crate-level warn carries an
//! `allow` only for `sched/`, `iommu/` and `log/` now.
//!
//! Where the machine access lives: [`cpu`] is one instruction per function and
//! the bottom of the tree, [`percpu`] owns `PerCpu`, its `gs:` offsets and the
//! `const`-generic primitives every GS access in the kernel goes through, and
//! [`entry`] owns the bracket — including the `cld` every Ring 0 entry owes
//! itself. Outside this directory nothing writes a `gs:` string, and which of
//! [`cpu`]'s wrappers are `unsafe fn` — with the argument for each one that
//! takes a caller-chosen value and is safe anyway — is that module's header's
//! subject.

pub mod apic;
pub mod control_regs;
pub mod cpu;
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
/// **IF is clear, and TF cannot be set.** IF excludes IRQ delivery and scheduler
/// preemption, and `cli` is what this closes with. TF used to be cleared here
/// too, and had to be: Ring 3 could set it, `SYSCALL` did not mask it, and the
/// `#DB` handler logged before returning — so a single-stepping thread could
/// reserve a whole newer generation while the interrupted writer was halfway
/// through its slot body. **Neither half is true any more, so the second write
/// is gone rather than kept for safety.** `emit` is kernel-only, and the two
/// ways into Ring 0 both settle the bit before a kernel instruction runs:
/// `arch::syscall::init` names TF in `IA32_FMASK`, and every interrupt and trap
/// gate clears it (SDM Vol. 3A §6.12.1). No Ring 0 code in this kernel runs with
/// TF set, and a `#DB` from Ring 3 ends the process rather than reporting.
///
/// The bracket is deliberately narrower than formatting: it covers only the
/// shard pointer and identity reads, the unlocked `xadd`, and the body
/// publication — three identity words plus the message's own `ceil(len/8)`, at
/// most 1,016 bytes and in practice nine words. It takes no lock and performs
/// no locked read-modify-write.
///
/// **What is measured, and what is argued.** `log-unbracketed-reserve` leaves
/// this type exactly as it is and makes it mask nothing;
/// `log_reserve_window_negative` boots it beside `log-nested-reserve` at
/// `--smp 8` and the log gate refuses the shard by name, because a handler that
/// logs from inside the reservation window takes the sequence numbers below the
/// one the interrupted producer had already stamped a timestamp for — and
/// `log_reserve_window` is the same boot with the `cli` back. That is the
/// *interrupt* half. The **migration** half is argued and not measured, and on
/// this kernel it cannot be: preemption here is deferred, `arch::idt`'s
/// `common_entry` returns to a Ring 0 frame without polling `need_resched`, and
/// nothing between `close` and the guard's drop reaches `preempt::enable` — so
/// no Ring 0 producer is switched out inside this bracket at any rate, and only
/// a Ready task ever migrates. The `cli` still buys it, and it buys it for a
/// scheduler that does not exist yet.
/// `issues/kernel/a-ring-0-loop-is-never-preempted.md`.
#[must_use = "dropping the log commit guard reopens interrupts and single-step traps"]
pub(crate) struct LogCommitGuard {
    rflags: u64,
    /// Restoring saved RFLAGS is a same-CPU operation. Keep safe code from
    /// moving this guard to another CPU before Drop.
    _not_send_sync: core::marker::PhantomData<*mut ()>,
}

impl LogCommitGuard {
    pub fn close() -> Self {
        let rflags: u64;
        // SAFETY: two blocks' worth of one argument, so it is written once
        // here. `pushfq`/`pop` is balanced, so `rsp` ends where it started and
        // no `nostack` is claimed; `cli` writes one `RFLAGS` bit and touches no
        // memory. **Irreducible by sequence**: reading `RFLAGS` and clearing IF
        // must be one uninterruptible run, which is the same reason
        // `hw::IrqGuard` cannot be built out of `cpu::` calls either. The
        // `popfq` in `Drop` restores exactly the word `close` read.
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
            // is now, and it masks nothing — so an interrupt that logs lands
            // where it was raised rather than at the drop, and a producer can
            // resume its body copy after a whole newer generation has committed
            // into the same slot. `log_reserve_window_negative` is what reads
            // it. In the shipping kernel the accessor is `const fn … { false }`
            // and this folds to the unconditional `cli` it replaced.
            if crate::actuator::log_unbracketed_reserve() {
                return Self { rflags, _not_send_sync: core::marker::PhantomData };
            }
            core::arch::asm!("cli");
        }
        Self { rflags, _not_send_sync: core::marker::PhantomData }
    }
}

impl Drop for LogCommitGuard {
    fn drop(&mut self) {
        // SAFETY: `close`'s argument — a balanced `push`/`popfq` writing back
        // the `RFLAGS` word this guard captured, on the CPU that captured it
        // (`_not_send_sync` is what keeps that true).
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
            // SAFETY: `sti` and `cli` write one `RFLAGS` bit each and touch no
            // memory. This whole branch is an actuator that exists to *stage* a
            // defect — the window between a load and a store — so what it is
            // doing is unsound by design and sound as machine code; in a
            // shipping kernel `log_shared_reservation()` is `const fn … { false }`
            // and none of it is emitted.
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
    // SAFETY: `counter.as_ptr()` is a live `&AtomicU64`, so the operand is a
    // naturally aligned, writable eight bytes for the whole block. The unlocked
    // `xadd` is atomic against an interrupt on this CPU because it retires
    // whole, and this function is `unsafe` precisely so its caller answers for
    // the other half — that no other CPU writes the word, with `guard` covering
    // the shard selection that established it. Irreducible: `fetch_add` is the
    // safe spelling and it is the *locked* instruction, which cost 350 ms of
    // boot under TCG (the doc comment above carries the measurement).
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
