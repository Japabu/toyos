// Monotonic clock using HPET (High Precision Event Timer).

use crate::drivers::mmio;
use crate::log;
use alloc::format;

// HPET register offsets
const HPET_CAP: u64 = 0x000;   // General Capabilities and ID (64-bit, RO)
const HPET_CFG: u64 = 0x010;   // General Configuration (64-bit, RW)
const HPET_COUNTER: u64 = 0x0F0; // Main Counter Value (64-bit, RW)

static mut HPET_BASE: u64 = 0;
static mut PERIOD_FS: u64 = 0; // counter period in femtoseconds

pub fn init(hpet_base: u64) {
    let cap = mmio::read_u64(hpet_base, HPET_CAP);
    let period_fs = cap >> 32;
    assert!(period_fs > 0, "HPET: invalid counter period");

    // Enable main counter (bit 0 of configuration register)
    let cfg = mmio::read_u64(hpet_base, HPET_CFG);
    mmio::write_u64(hpet_base, HPET_CFG, cfg | 1);

    unsafe {
        HPET_BASE = hpet_base;
        PERIOD_FS = period_fs;
    }

    let freq_hz = 1_000_000_000_000_000u64 / period_fs;
    log::println(&format!("HPET: period={}fs freq={}Hz", period_fs, freq_hz));
}

/// Returns nanoseconds since HPET was enabled.
pub fn nanos_since_boot() -> u64 {
    let base = unsafe { *(&raw const HPET_BASE) };
    if base == 0 {
        return 0;
    }
    let counter = mmio::read_u64(base, HPET_COUNTER);
    let period_fs = unsafe { *(&raw const PERIOD_FS) };
    // counter * period_fs gives femtoseconds; divide by 1_000_000 for nanoseconds
    // Use u128 to avoid overflow
    ((counter as u128 * period_fs as u128) / 1_000_000) as u64
}
