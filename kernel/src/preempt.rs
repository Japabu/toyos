//! Linux-style deferred preemption primitives.
//!
//! Two per-CPU words drive the model (defined in `arch::percpu::PerCpu`):
//!   - `preempt_count` @ gs:[240] — incremented by every IRQ entry and by
//!     `disable()`. Read-modify-writes are `lock`-prefixed because both kernel
//!     code and IRQ entries mutate it on the same CPU; `set_count`'s plain
//!     store needs no prefix (a naturally aligned 32-bit store, and a same-CPU
//!     IRQ cannot land inside one instruction).
//!   - `need_resched` @ gs:[244] — set by the timer ISR (and future wake
//!     paths), cleared by the deferred-preempt epilogue. Single-byte stores
//!     are naturally atomic on x86 — no `lock` prefix needed.
//!
//! When `enable()` drops the count to zero AND `need_resched` is set, the
//! slow path runs `scheduler::do_preempt()` to actually yield.
//!
//! Every GS-relative access in this file goes through `arch::percpu::gs`'s
//! `const`-generic primitives — `read_u32`, `write_u32`, `read_u8`,
//! `write_u8_imm`, `lock_inc_u32`, `lock_dec_u32` — rather than a hand-written
//! `asm!` string per accessor. The offset is a `const` operand, so each still
//! assembles to the immediate-displacement form (`lock addl $1, %gs:240`) the
//! entry stubs in `arch::syscall` and `arch::idt` open and close the same count
//! with. **They live in `arch::percpu` and not here**: that module declares
//! `PerCpu`, asserts every offset against the number the assembly hardcodes, and
//! reaches the same fields itself — this file had a second copy of both the
//! primitives and the three offsets, and a `gs:` string at the crate root is
//! also x86 in a file that is not `arch/`.
//!
//! The word is per-CPU but the depth it holds belongs to the running *context*,
//! so `Hw::switch` swaps it with the incoming context's saved depth. Without
//! that swap the count is not conserved across a switch and its absolute value
//! means nothing — which is what `scheduler.rs`'s §6.4 baselines rest on.

use core::sync::atomic::Ordering;

use crate::arch::percpu::{gs, OFF_FAULT_STATE, OFF_NEED_RESCHED, OFF_PREEMPT_COUNT};

/// Are the per-CPU preempt fields safe to touch yet? Cleared at boot, set by
/// `percpu::init_bsp` after writing IA32_GS_BASE. Before this, `gs:[N]` would
/// read from linear address N (low identity-mapped memory) — corruption hazard.
///
/// **This is the caller's half of every [`gs`] primitive's contract**, and this
/// module is the one that runs before `percpu::init_bsp`: `percpu`'s own
/// accessors do not ask, because nothing calls them that early.
#[inline]
fn percpu_ready() -> bool {
    crate::log::PERCPU_READY.load(Ordering::Relaxed)
}

#[inline]
pub fn count() -> u32 {
    if !percpu_ready() { return 0; }
    gs::read_u32::<OFF_PREEMPT_COUNT>()
}

/// Load the depth the context being switched *to* left behind.
///
/// The count is a per-CPU word but the depth it counts is per *context*: a task
/// that parks two levels deep inside a syscall owes two `enable`s, while a task
/// preempted at IRQ exit owes one, and the idle context owes one. Handing the
/// word over unchanged at the switch would therefore credit the incoming
/// context with the outgoing one's depth. Every context carries its own depth
/// in its `KernelCtx` instead, and `Hw::switch` swaps it with the word.
#[inline]
pub fn set_count(v: u32) {
    if !percpu_ready() { return; }
    gs::write_u32::<OFF_PREEMPT_COUNT>(v);
}

#[inline]
pub fn need_resched() -> bool {
    if !percpu_ready() { return false; }
    gs::read_u8::<OFF_NEED_RESCHED>() != 0
}

#[inline]
pub fn set_need_resched() {
    if !percpu_ready() { return; }
    gs::write_u8_imm::<OFF_NEED_RESCHED, 1>();
}

#[inline]
pub fn clear_need_resched() {
    if !percpu_ready() { return; }
    gs::write_u8_imm::<OFF_NEED_RESCHED, 0>();
}

#[inline]
pub fn disable() {
    if !percpu_ready() { return; }
    gs::lock_inc_u32::<OFF_PREEMPT_COUNT>();
}

/// Drop the count without polling `need_resched`, for a caller that is about
/// to reschedule anyway (the wait ticket's park — see `waitq`). The request
/// stays set, so nothing is dropped: the imminent `do_schedule` serves it, and
/// if the caller changes its mind the next poll picks it up.
#[inline]
pub fn enable_no_resched() {
    if !percpu_ready() { return; }
    gs::lock_dec_u32::<OFF_PREEMPT_COUNT>();
}

#[inline]
pub fn enable() {
    if !percpu_ready() { return; }
    gs::lock_dec_u32::<OFF_PREEMPT_COUNT>();
    // `do_preempt` does the clear itself, gated on the in-schedule re-entry
    // guard: if we're nested inside a `do_schedule` frame it returns
    // without clearing, so the next non-nested poll picks up the request.
    // Eager-clearing here would silently drop preempt requests that fired
    // during the outer schedule's resume path — and since timers are
    // one-shot, a dropped request means the task runs without preemption
    // until something else interrupts it.
    if count() == 0 && need_resched() && !faulting() {
        crate::scheduler::do_preempt();
    }
}

/// Whether this CPU is inside a fault or panic report.
///
/// `gs:[256]` is `PerCpu::fault_state`, non-zero for PageFault/Fatal/Panic and
/// asserted at that offset in `percpu.rs` alongside the other raw offsets this
/// module uses.
///
/// A CPU inside a report is not reschedulable, so a `fault_state` never
/// returned to Normal costs that CPU its preemption for the rest of the boot:
/// a leak here is a hang, not a nuisance.
#[inline]
fn faulting() -> bool {
    if !percpu_ready() { return false; }
    gs::read_u8::<OFF_FAULT_STATE>() != 0
}
