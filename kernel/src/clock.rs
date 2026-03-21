// Monotonic clock: calibrates TSC against HPET at boot, then uses TSC for fast reads.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use crate::arch::cpu;

// HPET register offsets
const HPET_CAP: u64 = 0x000;
const HPET_CFG: u64 = 0x010;
const HPET_COUNTER: u64 = 0x0F0;

static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_PERIOD_FS: AtomicU64 = AtomicU64::new(0);

pub fn init(hpet_base: u64) {
    let hpet = crate::mm::paging::kernel().lock().as_mut().unwrap().map_mmio(hpet_base, 0x1000);

    let cap = hpet.read_u64(HPET_CAP);
    let hpet_period_fs = cap >> 32;
    assert!(hpet_period_fs > 0, "HPET: invalid counter period");

    // Enable HPET main counter
    let cfg = hpet.read_u64(HPET_CFG);
    hpet.write_u64(HPET_CFG, cfg | 1);

    // Calibrate TSC: measure TSC ticks over ~50ms of HPET time
    let calibration_ns: u64 = 50_000_000; // 50ms
    let calibration_hpet_ticks = calibration_ns * 1_000_000 / hpet_period_fs;

    let hpet_start = hpet.read_u64(HPET_COUNTER);
    let tsc_start = cpu::rdtsc();
    let hpet_target = hpet_start + calibration_hpet_ticks;
    while hpet.read_u64(HPET_COUNTER) < hpet_target {}
    let tsc_end = cpu::rdtsc();
    let hpet_end = hpet.read_u64(HPET_COUNTER);

    let hpet_elapsed_fs = (hpet_end - hpet_start) as u128 * hpet_period_fs as u128;
    let tsc_delta = tsc_end - tsc_start;
    let tsc_period_fs = (hpet_elapsed_fs / tsc_delta as u128) as u64;

    TSC_BOOT.store(tsc_start, Relaxed);
    TSC_PERIOD_FS.store(tsc_period_fs, Relaxed);

    let tsc_freq_mhz = 1_000_000_000_000_000u64 / tsc_period_fs / 1_000_000;
    log!("TSC: {}MHz (period={}fs, calibrated over {}ms)", tsc_freq_mhz, tsc_period_fs, calibration_ns / 1_000_000);
}

/// Returns nanoseconds since boot. Lock-free, no MMIO.
pub fn nanos_since_boot() -> u64 {
    let delta = cpu::rdtsc() - TSC_BOOT.load(Relaxed);
    let period_fs = TSC_PERIOD_FS.load(Relaxed);
    ((delta as u128 * period_fs as u128) / 1_000_000) as u64
}
