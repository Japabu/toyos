//! Linux-style deferred preemption primitives.
//!
//! Two per-CPU words drive the model (defined in `arch::percpu::PerCpu`):
//!   - `preempt_count` @ gs:[240] — incremented by every IRQ entry and by
//!     `disable()`. `lock`-prefixed because both kernel code and IRQ entries
//!     mutate it on the same CPU.
//!   - `need_resched` @ gs:[244] — set by the timer ISR (and future wake
//!     paths), cleared by the deferred-preempt epilogue. Single-byte stores
//!     are naturally atomic on x86 — no `lock` prefix needed.
//!
//! When `enable()` drops the count to zero AND `need_resched` is set, the
//! slow path runs `scheduler::do_preempt()` to actually yield.

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
