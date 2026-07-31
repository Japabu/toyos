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
//! The word is per-CPU but the depth it holds belongs to the running *context*,
//! so `Hw::switch` swaps it with the incoming context's saved depth. Without
//! that swap the count is not conserved across a switch and its absolute value
//! means nothing — which is what `scheduler.rs`'s §6.4 baselines rest on.

use core::arch::asm;
use core::sync::atomic::Ordering;

/// Are the per-CPU preempt fields safe to touch yet? Cleared at boot, set by
/// `percpu::init_bsp` after writing IA32_GS_BASE. Before this, `gs:[N]` would
/// read from linear address N (low identity-mapped memory) — corruption hazard.
#[inline]
fn percpu_ready() -> bool {
    crate::log::PERCPU_READY.load(Ordering::Relaxed)
}

#[inline]
pub fn count() -> u32 {
    if !percpu_ready() { return 0; }
    let v: u32;
    unsafe {
        asm!("mov {:e}, gs:[240]", out(reg) v,
            options(nomem, nostack, preserves_flags));
    }
    v
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
    unsafe {
        asm!("mov gs:[240], {:e}", in(reg) v,
            options(nostack, preserves_flags));
    }
}

#[inline]
pub fn need_resched() -> bool {
    if !percpu_ready() { return false; }
    let v: u8;
    unsafe {
        asm!("mov {}, gs:[244]", out(reg_byte) v,
            options(nomem, nostack, preserves_flags));
    }
    v != 0
}

#[inline]
pub fn set_need_resched() {
    if !percpu_ready() { return; }
    unsafe {
        asm!("mov byte ptr gs:[244], 1",
            options(nomem, nostack, preserves_flags));
    }
}

#[inline]
pub fn clear_need_resched() {
    if !percpu_ready() { return; }
    unsafe {
        asm!("mov byte ptr gs:[244], 0",
            options(nomem, nostack, preserves_flags));
    }
}

#[inline]
pub fn disable() {
    if !percpu_ready() { return; }
    unsafe {
        asm!("lock add dword ptr gs:[240], 1",
            options(nostack, preserves_flags));
    }
}

/// Drop the count without polling `need_resched`, for a caller that is about
/// to reschedule anyway (the wait ticket's park — see `waitq`). The request
/// stays set, so nothing is dropped: the imminent `do_schedule` serves it, and
/// if the caller changes its mind the next poll picks it up.
#[inline]
pub fn enable_no_resched() {
    if !percpu_ready() { return; }
    unsafe {
        asm!("lock sub dword ptr gs:[240], 1",
            options(nostack, preserves_flags));
    }
}

#[inline]
pub fn enable() {
    if !percpu_ready() { return; }
    unsafe {
        asm!("lock sub dword ptr gs:[240], 1",
            options(nostack, preserves_flags));
    }
    // `do_preempt` does the clear itself, gated on the in-schedule re-entry
    // guard: if we're nested inside a `do_schedule` frame it returns
    // without clearing, so the next non-nested poll picks up the request.
    // Eager-clearing here would silently drop preempt requests that fired
    // during the outer schedule's resume path — and since timers are
    // one-shot, a dropped request means the task runs without preemption
    // until something else interrupts it.
    if count() == 0 && need_resched() {
        crate::scheduler::do_preempt();
    }
}
