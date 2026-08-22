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
//! Every GS-relative access in this file goes through one of six `const`-generic
//! primitives near the top — `read_u32`, `write_u32`, `read_u8`, `write_u8`,
//! `lock_inc_u32`, `lock_dec_u32` — rather than a hand-written `asm!` string per
//! accessor. The offset is a `const` operand, so each still assembles to the
//! immediate-displacement form (`lock addl $1, %gs:240`) the entry stubs in
//! `arch::syscall` and `arch::idt` open and close the same count with.
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

/// `PerCpu::preempt_count`, asserted at this offset in `arch::percpu`.
const PREEMPT_COUNT: u32 = 240;
/// `PerCpu::need_resched`, asserted at this offset in `arch::percpu`.
const NEED_RESCHED: u32 = 244;
/// `PerCpu::fault_state`, asserted at this offset in `arch::percpu`.
const FAULT_STATE: u32 = 256;

// The six primitives below are the whole of this module's `unsafe`, and they
// are six rather than the nine hand-written `asm!` strings they replace: the
// offset is a `const` operand, so `gs:[{off}]` assembles to the same immediate
// displacement `gs:[240]` did and no register is spent reaching it. Every one
// of them is irreducible in the same way — a GS-relative access is a machine
// facility with no Rust operation behind it, and `PerCpu` cannot be a `static`
// because each CPU needs a different one at the same name.
//
// What the caller owes, and what none of these can check for itself: `GS_BASE`
// must already point at this CPU's `PerCpu`. Every caller in this file asks
// `percpu_ready()` first, which is the only reason any of it is sound before
// `percpu::init_bsp` runs.

/// One naturally aligned per-CPU `u32` load.
#[inline]
fn read_u32<const OFF: u32>() -> u32 {
    let v: u32;
    // SAFETY: `OFF` is one of the three constants above, each of which names a
    // field `arch::percpu` asserts at that offset inside `PerCpu`, and the
    // caller has checked `percpu_ready()` — so `GS_BASE + OFF` is a live,
    // naturally aligned word of this CPU's own `PerCpu`. `nomem` because the
    // access reaches no memory any Rust value names; `preserves_flags` because
    // `mov` writes none.
    unsafe {
        asm!("mov {v:e}, gs:[{off}]", v = out(reg) v, off = const OFF,
            options(nomem, nostack, preserves_flags));
    }
    v
}

/// One naturally aligned per-CPU `u32` store. No `lock` prefix: a 32-bit store
/// to an aligned address is atomic on x86, and a same-CPU IRQ cannot land
/// inside one instruction.
#[inline]
fn write_u32<const OFF: u32>(v: u32) {
    // SAFETY: same argument as `read_u32`, minus `nomem` — this one does write
    // the word, and the ISR that also writes it is on this CPU, so the store's
    // own atomicity is the whole of the synchronization.
    unsafe {
        asm!("mov gs:[{off}], {v:e}", off = const OFF, v = in(reg) v,
            options(nostack, preserves_flags));
    }
}

/// One per-CPU byte load.
#[inline]
fn read_u8<const OFF: u32>() -> u8 {
    let v: u8;
    // SAFETY: same argument as `read_u32`, for one byte.
    unsafe {
        asm!("mov {v}, gs:[{off}]", v = out(reg_byte) v, off = const OFF,
            options(nomem, nostack, preserves_flags));
    }
    v
}

/// One per-CPU byte store, from an immediate. Single-byte stores are naturally
/// atomic on x86 — no `lock` prefix needed.
#[inline]
fn write_u8<const OFF: u32, const VAL: u8>() {
    // SAFETY: same argument as `write_u32`, for one byte.
    unsafe {
        asm!("mov byte ptr gs:[{off}], {val}", off = const OFF, val = const VAL,
            options(nostack, preserves_flags));
    }
}

/// One `lock`-prefixed increment of a per-CPU `u32`.
///
/// The prefix is not optional and is why the two counter primitives are their
/// own: both kernel code and IRQ entry read-modify-write `preempt_count` on the
/// same CPU, and an interrupt landing between the load and the store of a plain
/// `add` loses whichever side went second.
///
/// Increment and decrement stay two functions rather than one taking a delta,
/// so the instruction is still the immediate-form `lock add`/`lock sub` every
/// description of this path names — `arch::syscall`'s and `arch::idt`'s entry
/// stubs open and close the same count with the same two instructions.
#[inline]
fn lock_inc_u32<const OFF: u32>() {
    // SAFETY: same argument as `write_u32`. **No `preserves_flags`**, and that
    // is a fix rather than an omission: `lock add` writes OF, SF, ZF, AF, CF
    // and PF, so the call sites that used to claim it were telling the
    // compiler it could keep a comparison's result live across a preempt-count
    // change. Nothing was observed miscompiling from it; the claim was simply
    // false, and writing this comment is what found it.
    unsafe {
        asm!("lock add dword ptr gs:[{off}], 1", off = const OFF,
            options(nostack));
    }
}

/// One `lock`-prefixed decrement of a per-CPU `u32`. See [`lock_inc_u32`].
#[inline]
fn lock_dec_u32<const OFF: u32>() {
    // SAFETY: `lock_inc_u32`'s argument exactly, including why
    // `preserves_flags` is absent.
    unsafe {
        asm!("lock sub dword ptr gs:[{off}], 1", off = const OFF,
            options(nostack));
    }
}

#[inline]
pub fn count() -> u32 {
    if !percpu_ready() { return 0; }
    read_u32::<PREEMPT_COUNT>()
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
    write_u32::<PREEMPT_COUNT>(v);
}

#[inline]
pub fn need_resched() -> bool {
    if !percpu_ready() { return false; }
    read_u8::<NEED_RESCHED>() != 0
}

#[inline]
pub fn set_need_resched() {
    if !percpu_ready() { return; }
    write_u8::<NEED_RESCHED, 1>();
}

#[inline]
pub fn clear_need_resched() {
    if !percpu_ready() { return; }
    write_u8::<NEED_RESCHED, 0>();
}

#[inline]
pub fn disable() {
    if !percpu_ready() { return; }
    lock_inc_u32::<PREEMPT_COUNT>();
}

/// Drop the count without polling `need_resched`, for a caller that is about
/// to reschedule anyway (the wait ticket's park — see `waitq`). The request
/// stays set, so nothing is dropped: the imminent `do_schedule` serves it, and
/// if the caller changes its mind the next poll picks it up.
#[inline]
pub fn enable_no_resched() {
    if !percpu_ready() { return; }
    lock_dec_u32::<PREEMPT_COUNT>();
}

#[inline]
pub fn enable() {
    if !percpu_ready() { return; }
    lock_dec_u32::<PREEMPT_COUNT>();
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
    read_u8::<FAULT_STATE>() != 0
}
