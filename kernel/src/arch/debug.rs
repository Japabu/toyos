//! What the `#DB` handler reads out of the debug registers.
//!
//! **This is not a place to arm one from.** It carried five investigation tools
//! with zero callers each — `set_context`, `watch_write`, `clear`,
//! `monitor_pte` and a timer-tick PTE poller — kept behind
//! `arch/mod.rs`'s `#[allow(dead_code)]` for a bring-up that had ended.
//! `issues/design-debt/four-deletions-still-owed.md` named them and git
//! history is the shelf: a session that wants a hardware watchpoint sets it
//! from the debugger, and one that genuinely wants the guest to arm its own
//! writes it against the DR7 encoding in the manual rather than inheriting a
//! dead one.
//!
//! What is left is the read side, which the handler in `arch/idt/exceptions.rs`
//! calls on every debug exception however that exception was armed.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// What triggered the watchpoint — logged by the #DB handler.
///
/// Nothing stores to it since the arming tools went, so the handler reports
/// zero; whoever adds a tool that arms a watchpoint adds the store back beside
/// it, which is the only moment the tag means anything.
static WATCH0_CONTEXT: AtomicU64 = AtomicU64::new(0);

/// Get the stored context tag.
pub fn context() -> u64 {
    WATCH0_CONTEXT.load(Ordering::Relaxed)
}

/// Read DR6 (debug status — which breakpoint fired and why).
pub fn read_dr6() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, dr6", out(reg) val); }
    val
}
