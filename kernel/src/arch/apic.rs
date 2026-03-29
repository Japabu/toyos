use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::cpu;
use crate::log;

// x2APIC MSR addresses (base 0x800 + xAPIC_offset >> 4)
const IA32_APIC_BASE_MSR: u32 = 0x1B;
const X2APIC_ID: u32 = 0x802;
const X2APIC_SVR: u32 = 0x80F;
const X2APIC_EOI: u32 = 0x80B;
const X2APIC_ICR: u32 = 0x830;
const X2APIC_LVT_TIMER: u32 = 0x832;
const X2APIC_TIMER_INIT: u32 = 0x838;
const X2APIC_TIMER_CURRENT: u32 = 0x839;
const X2APIC_TIMER_DIVIDE: u32 = 0x83E;

pub const TIMER_VECTOR: u8 = 0x20;

/// Calibrated LAPIC timer ticks per 10ms (computed on BSP, reused by APs).
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

/// Set once during BSP init. Guards IPI sends before APIC is ready.
static X2APIC_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable x2APIC mode on this CPU. Sets global enable (bit 11) + x2APIC (bit 10)
/// in IA32_APIC_BASE, then software-enables via SVR.
fn enable_x2apic() {
    let mut base = cpu::rdmsr(IA32_APIC_BASE_MSR);
    base |= (1 << 11) | (1 << 10);
    cpu::wrmsr(IA32_APIC_BASE_MSR, base);

    let svr = cpu::rdmsr(X2APIC_SVR);
    cpu::wrmsr(X2APIC_SVR, svr | (1 << 8) | 0xFF);
}

/// Initialize the BSP's Local APIC in x2APIC mode.
pub fn init() {
    enable_x2apic();
    X2APIC_ENABLED.store(true, Ordering::Release);
    log!("LAPIC: x2APIC enabled (ID {})", id());
}

/// Enable the AP's local APIC in x2APIC mode.
pub fn init_ap() {
    enable_x2apic();
}

pub fn id() -> u32 {
    cpu::rdmsr(X2APIC_ID) as u32
}

/// Send INIT IPI to the specified APIC ID.
pub fn send_init(apic_id: u32) {
    // ICR: destination in high 32 bits, 0x4500 = delivery INIT, level assert
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4500);
}

/// Send Startup IPI (SIPI) with the given vector (trampoline page number).
pub fn send_sipi(apic_id: u32, vector: u8) {
    cpu::wrmsr(X2APIC_ICR, ((apic_id as u64) << 32) | 0x4600 | vector as u64);
}

/// Send EOI.
#[inline]
pub fn eoi() {
    cpu::wrmsr(X2APIC_EOI, 0);
}

/// Send an IPI to all CPUs except self (shorthand destination).
fn ipi_all_excluding_self(vector: u8) {
    // destination shorthand = all-excluding-self (0b11 << 18), fixed delivery
    cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | vector as u64);
}

/// Flush TLB on all other CPUs. No-op if x2APIC not yet initialized.
pub fn tlb_shootdown() {
    if X2APIC_ENABLED.load(Ordering::Relaxed) {
        ipi_all_excluding_self(0xFE);
    }
}

/// Send a timer IPI to all other CPUs, waking any that are halted.
pub fn kick_cpus() {
    if !X2APIC_ENABLED.load(Ordering::Relaxed) { return; }
    cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | TIMER_VECTOR as u64);
}

/// Halt all CPUs. Sends halt IPI to all other CPUs, then halts self.
pub fn halt_all_cpus() -> ! {
    if X2APIC_ENABLED.load(Ordering::Relaxed) {
        cpu::wrmsr(X2APIC_ICR, 0x000C_0000 | 0xFD);
    }
    super::cpu::halt();
}

/// Calibrate the LAPIC timer on the BSP. Requires HPET.
/// Does not start the timer — the scheduler arms one-shot timers on demand.
pub fn init_timer() {
    // Divide by 1 for maximum resolution
    cpu::wrmsr(X2APIC_TIMER_DIVIDE, 0b1011);

    // Masked one-shot mode for calibration
    cpu::wrmsr(X2APIC_LVT_TIMER, 1 << 16);
    cpu::wrmsr(X2APIC_TIMER_INIT, 0xFFFF_FFFF);

    let start = crate::clock::nanos_since_boot();
    while crate::clock::nanos_since_boot() - start < 10_000_000 {}
    let elapsed = crate::clock::nanos_since_boot() - start;

    let remaining = cpu::rdmsr(X2APIC_TIMER_CURRENT) as u32;
    let ticks_elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);
    let ticks_10ms = (ticks_elapsed as u64 * 10_000_000 / elapsed) as u32;

    cpu::wrmsr(X2APIC_TIMER_INIT, 0);
    TIMER_TICKS.store(ticks_10ms, Ordering::Release);
    log!("LAPIC timer: {} ticks/10ms", ticks_10ms);
}

/// AP timer init — calibration was done on the BSP, nothing to start.
pub fn init_timer_ap() {}

/// Arm a one-shot timer to fire after `nanos` nanoseconds.
pub fn arm_one_shot(nanos: u64) {
    let ticks_10ms = TIMER_TICKS.load(Ordering::Relaxed) as u64;
    let ticks = (nanos as u128 * ticks_10ms as u128 / 10_000_000) as u64;
    let ticks = ticks.clamp(1, u32::MAX as u64) as u32;
    cpu::wrmsr(X2APIC_TIMER_DIVIDE, 0b1011);
    cpu::wrmsr(X2APIC_LVT_TIMER, TIMER_VECTOR as u64);
    cpu::wrmsr(X2APIC_TIMER_INIT, ticks as u64);
}

/// Stop the timer. No more interrupts until re-armed.
pub fn stop_timer() {
    cpu::wrmsr(X2APIC_TIMER_INIT, 0);
}
