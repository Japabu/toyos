//! x86-64 hardware watchpoints via debug registers (DR0-DR7).
//!
//! Also provides a polling-based PTE monitor that checks a stored PTE address
//! on every timer tick. Useful when hardware watchpoints cause issues.

use core::arch::asm;
use core::sync::atomic::{AtomicU64, Ordering};

/// What triggered the watchpoint — logged by the #DB handler.
static WATCH0_CONTEXT: AtomicU64 = AtomicU64::new(0);

/// Store a context tag so the #DB handler can log what we're watching.
pub fn set_context(tag: u64) {
    WATCH0_CONTEXT.store(tag, Ordering::Relaxed);
}

/// Get the stored context tag.
pub fn context() -> u64 {
    WATCH0_CONTEXT.load(Ordering::Relaxed)
}

/// DR7 encoding helpers.
const DR7_LOCAL_ENABLE_DR0: u64 = 1 << 0;
const DR7_GLOBAL_ENABLE_DR0: u64 = 1 << 1;
const DR7_CONDITION_SHIFT_DR0: u64 = 16;
const DR7_LENGTH_SHIFT_DR0: u64 = 18;
const CONDITION_WRITE: u64 = 0b01;
const LENGTH_8_BYTES: u64 = 0b10;

/// Set DR0 to watch for writes to `addr` (8-byte region, must be 8-byte aligned).
pub fn watch_write(addr: u64) {
    unsafe {
        let zero: u64 = 0;
        asm!("mov dr6, {}", in(reg) zero);
        asm!("mov dr0, {}", in(reg) addr);
        let dr7 = DR7_LOCAL_ENABLE_DR0 | DR7_GLOBAL_ENABLE_DR0
            | (CONDITION_WRITE << DR7_CONDITION_SHIFT_DR0)
            | (LENGTH_8_BYTES << DR7_LENGTH_SHIFT_DR0);
        asm!("mov dr7, {}", in(reg) dr7);
    }
    crate::log!("debug: watching writes to {:#x}", addr);
}

/// Disable all hardware watchpoints.
pub fn clear() {
    unsafe {
        let zero: u64 = 0;
        asm!("mov dr7, {}", in(reg) zero);
        asm!("mov dr6, {}", in(reg) zero);
    }
}

/// Read DR6 (debug status — which breakpoint fired and why).
pub fn read_dr6() -> u64 {
    let val: u64;
    unsafe { asm!("mov {}, dr6", out(reg) val); }
    val
}

// --- Polling PTE monitor ---

/// Address of a PTE to monitor. 0 = disabled.
static MONITOR_PTE_ADDR: AtomicU64 = AtomicU64::new(0);
/// Expected value of the PTE (set when monitoring starts).
static MONITOR_PTE_EXPECTED: AtomicU64 = AtomicU64::new(0);

/// Start monitoring a PTE address. The timer tick will check if it changes.
pub fn monitor_pte(pte_addr: u64) {
    let val = unsafe { *(pte_addr as *const u64) };
    MONITOR_PTE_EXPECTED.store(val, Ordering::Relaxed);
    MONITOR_PTE_ADDR.store(pte_addr, Ordering::Release);
    crate::log!("debug: monitoring PTE at {:#x} (expected={:#x})", pte_addr, val);
}

/// Called from the timer handler. Checks if the monitored PTE has changed.
/// If it has, logs the corruption and disables the monitor.
pub fn check_pte_monitor() {
    let addr = MONITOR_PTE_ADDR.load(Ordering::Acquire);
    if addr == 0 { return; }

    let current = unsafe { *(addr as *const u64) };
    let expected = MONITOR_PTE_EXPECTED.load(Ordering::Relaxed);

    // Mask out Accessed (bit 5) and Dirty (bit 6) — set by CPU hardware
    let mask = !0x60u64;
    if current & mask != expected & mask {
        // PTE changed (ignoring A/D bits)! Disable monitor and report
        MONITOR_PTE_ADDR.store(0, Ordering::Relaxed);

        crate::log!("!!! PTE CORRUPTION DETECTED !!!");
        crate::log!("  PTE addr={:#x}", addr);
        crate::log!("  expected={:#018x}", expected);
        crate::log!("  actual  ={:#018x}", current);
        crate::log!("  current pid={:?}", crate::arch::percpu::current_pid());

        // Dump what's at the PT page — are neighboring PTEs also corrupted?
        let pt_base = addr & !0xFFF;
        let pt_idx = ((addr - pt_base) / 8) as usize;
        crate::log!("  PT base={:#x}, corrupted index={}", pt_base, pt_idx);

        // Check a range around the corrupted entry
        let start = if pt_idx >= 4 { pt_idx - 4 } else { 0 };
        let end = if pt_idx + 4 < 512 { pt_idx + 4 } else { 511 };
        for i in start..=end {
            let val = unsafe { *((pt_base + i as u64 * 8) as *const u64) };
            let marker = if i == pt_idx { " <-- CORRUPTED" } else { "" };
            crate::log!("  PT[{}] = {:#018x}{}", i, val, marker);
        }
    }
}
