// Monotonic clock using HPET (High Precision Event Timer).

use crate::drivers::mmio::Mmio;
use crate::log;
use crate::sync::Lock;

// HPET register offsets
const HPET_CAP: u64 = 0x000;   // General Capabilities and ID (64-bit, RO)
const HPET_CFG: u64 = 0x010;   // General Configuration (64-bit, RW)
const HPET_COUNTER: u64 = 0x0F0; // Main Counter Value (64-bit, RW)

static HPET: Lock<Option<Mmio>> = Lock::new(None);
static PERIOD_FS: Lock<u64> = Lock::new(0); // counter period in femtoseconds

pub fn init(hpet_base: u64) {
    let hpet = Mmio::new(hpet_base);

    let cap = hpet.read_u64(HPET_CAP);
    let period_fs = cap >> 32;
    assert!(period_fs > 0, "HPET: invalid counter period");

    // Enable main counter (bit 0 of configuration register)
    let cfg = hpet.read_u64(HPET_CFG);
    hpet.write_u64(HPET_CFG, cfg | 1);

    *HPET.get_mut() = Some(hpet);
    *PERIOD_FS.get_mut() = period_fs;

    let freq_hz = 1_000_000_000_000_000u64 / period_fs;
    log!("HPET: period={}fs freq={}Hz", period_fs, freq_hz);
}

/// Returns nanoseconds since HPET was enabled.
pub fn nanos_since_boot() -> u64 {
    let Some(hpet) = *HPET.get() else { return 0; };
    let counter = hpet.read_u64(HPET_COUNTER);
    let period_fs = *PERIOD_FS.get();
    // counter * period_fs gives femtoseconds; divide by 1_000_000 for nanoseconds
    // Use u128 to avoid overflow
    ((counter as u128 * period_fs as u128) / 1_000_000) as u64
}
