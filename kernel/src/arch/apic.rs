use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::mm::Mmio;
use crate::log;
use crate::sync::Lock;

// Local APIC register offsets
const LAPIC_ID: u64 = 0x020;
const LAPIC_SVR: u64 = 0x0F0;
const LAPIC_ICR_LOW: u64 = 0x300;
const LAPIC_ICR_HIGH: u64 = 0x310;
const LAPIC_EOI: u64 = 0x0B0;
const LAPIC_LVT_TIMER: u64 = 0x320;
const LAPIC_TIMER_INIT: u64 = 0x380;
const LAPIC_TIMER_CURRENT: u64 = 0x390;
const LAPIC_TIMER_DIVIDE: u64 = 0x3E0;

pub const TIMER_VECTOR: u8 = 0x20;

/// Calibrated LAPIC timer ticks per 10ms (computed on BSP, reused by APs).
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

static LAPIC: Lock<Option<Mmio>> = Lock::new(None);

/// LAPIC base address for lock-free access in interrupt handlers.
/// Set once during init, read-only afterwards.
/// `#[no_mangle]` so the TLB flush handler in idt.rs can reference it via `rip + LAPIC_BASE`.
#[no_mangle]
static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);

/// Initialize the BSP's Local APIC at the given physical address.
pub fn init(base_addr: u32) {
    let mmio = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(base_addr as u64, 0x1000);

    // Enable LAPIC: set SVR bit 8 (software enable) + spurious vector 0xFF
    mmio.write_u32(LAPIC_SVR, mmio.read_u32(LAPIC_SVR) | (1 << 8) | 0xFF);

    LAPIC_BASE.store(crate::DirectMap::from_phys(base_addr as u64).as_ptr::<u8>() as u64, Ordering::Release);
    *LAPIC.lock() = Some(mmio);
    log!("LAPIC: enabled (ID {})", id());
}

fn with_lapic<R>(f: impl FnOnce(&Mmio) -> R) -> R {
    let guard = LAPIC.lock();
    let mmio = guard.as_ref().expect("LAPIC not initialized");
    f(mmio)
}

pub fn id() -> u8 {
    (with_lapic(|l| l.read_u32(LAPIC_ID)) >> 24) as u8
}

/// Send INIT IPI to the specified APIC ID.
pub fn send_init(apic_id: u8) {
    with_lapic(|l| {
        l.write_u32(LAPIC_ICR_HIGH, (apic_id as u32) << 24);
        l.write_u32(LAPIC_ICR_LOW, 0x4500); // delivery=INIT, level=assert
    });
}

/// Send Startup IPI (SIPI) with the given vector (trampoline page number).
pub fn send_sipi(apic_id: u8, vector: u8) {
    with_lapic(|l| {
        l.write_u32(LAPIC_ICR_HIGH, (apic_id as u32) << 24);
        l.write_u32(LAPIC_ICR_LOW, 0x4600 | vector as u32); // delivery=Startup
    });
}

/// Enable the AP's local APIC (same MMIO base, already mapped by BSP).
pub fn init_ap() {
    with_lapic(|l| {
        l.write_u32(LAPIC_SVR, l.read_u32(LAPIC_SVR) | (1 << 8) | 0xFF);
    });
}

/// Send EOI. Lock-free — safe to call from interrupt handlers.
pub fn eoi() {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe { ((base + LAPIC_EOI) as *mut u32).write_volatile(0); }
}

/// Send an IPI to all CPUs except self (shorthand destination).
fn ipi_all_excluding_self(vector: u8) {
    with_lapic(|l| {
        // ICR: destination shorthand = all-excluding-self (0b11 << 18), fixed delivery
        l.write_u32(LAPIC_ICR_LOW, 0x000C_0000 | vector as u32);
    });
}

/// Flush TLB on all other CPUs. No-op if LAPIC not yet initialized.
pub fn tlb_shootdown() {
    if LAPIC_BASE.load(Ordering::Relaxed) != 0 {
        ipi_all_excluding_self(0xFE);
    }
}

/// Send a timer IPI to all other CPUs, waking any that are halted.
/// Lock-free — safe to call from scheduler context.
pub fn kick_cpus() {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    if base == 0 { return; }
    unsafe {
        // ICR: all-excluding-self (0b11 << 18) | timer vector
        (base as *mut u8).add(LAPIC_ICR_LOW as usize).cast::<u32>()
            .write_volatile(0x000C_0000 | TIMER_VECTOR as u32);
    }
}

/// Calibrate the LAPIC timer on the BSP. Requires HPET.
/// Does not start the timer — the scheduler arms one-shot timers on demand.
pub fn init_timer() {
    with_lapic(|l| {
        // Divide by 1 for maximum resolution
        l.write_u32(LAPIC_TIMER_DIVIDE, 0b1011);

        // Masked one-shot mode for calibration
        l.write_u32(LAPIC_LVT_TIMER, 1 << 16);
        l.write_u32(LAPIC_TIMER_INIT, 0xFFFF_FFFF);

        let start = crate::clock::nanos_since_boot();
        while crate::clock::nanos_since_boot() - start < 10_000_000 {}
        let elapsed = crate::clock::nanos_since_boot() - start;

        let remaining = l.read_u32(LAPIC_TIMER_CURRENT);
        let ticks_elapsed = 0xFFFF_FFFFu32.wrapping_sub(remaining);
        let ticks_10ms = (ticks_elapsed as u64 * 10_000_000 / elapsed) as u32;

        l.write_u32(LAPIC_TIMER_INIT, 0);
        TIMER_TICKS.store(ticks_10ms, Ordering::Release);
        log!("LAPIC timer: {} ticks/10ms", ticks_10ms);
    });
}

/// AP timer init — calibration was done on the BSP, nothing to start.
pub fn init_timer_ap() {}

/// Arm a one-shot timer to fire after `nanos` nanoseconds.
/// Lock-free — writes LAPIC MMIO directly. Safe to call from scheduler context.
pub fn arm_one_shot(nanos: u64) {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    let ticks_10ms = TIMER_TICKS.load(Ordering::Relaxed) as u64;
    let ticks = (nanos as u128 * ticks_10ms as u128 / 10_000_000) as u64;
    let ticks = ticks.clamp(1, u32::MAX as u64) as u32;
    unsafe {
        let base = base as *mut u8;
        // Divide by 1
        base.add(LAPIC_TIMER_DIVIDE as usize).cast::<u32>().write_volatile(0b1011);
        // One-shot mode (bit 17 = 0), unmasked, vector
        base.add(LAPIC_LVT_TIMER as usize).cast::<u32>().write_volatile(TIMER_VECTOR as u32);
        // Initial count — starts countdown
        base.add(LAPIC_TIMER_INIT as usize).cast::<u32>().write_volatile(ticks);
    }
}

/// Stop the timer. No more interrupts until re-armed.
pub fn stop_timer() {
    let base = LAPIC_BASE.load(Ordering::Relaxed);
    unsafe {
        (base as *mut u8).add(LAPIC_TIMER_INIT as usize).cast::<u32>().write_volatile(0);
    }
}
