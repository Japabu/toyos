// Monotonic clock using RDTSC, calibrated against PIT channel 2.

use core::arch::asm;

static mut TSC_FREQ: u64 = 0; // ticks per second
static mut TSC_BASE: u64 = 0; // RDTSC value at init

fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
    }
    (hi as u64) << 32 | lo as u64
}

/// Calibrate TSC frequency against PIT channel 2.
///
/// Programs PIT channel 2 for a ~10ms one-shot, measures RDTSC delta.
pub fn init() {
    // PIT oscillator frequency: 1,193,182 Hz
    const PIT_FREQ: u64 = 1_193_182;
    // Count for ~10ms: 1,193,182 * 10 / 1000 = 11,932
    const COUNT: u16 = 11_932;
    const EXPECTED_US: u64 = (COUNT as u64) * 1_000_000 / PIT_FREQ;

    unsafe {
        // Enable PIT channel 2 gate (bit 0 of port 0x61)
        let gate: u8;
        asm!("in al, 0x61", out("al") gate, options(nomem, nostack));
        let gate = (gate & 0xFD) | 0x01; // clear speaker (bit 1), set gate (bit 0)
        asm!("out 0x61, al", in("al") gate, options(nomem, nostack));

        // Program PIT channel 2: mode 0 (one-shot), lobyte/hibyte
        asm!("out 0x43, al", in("al") 0b10110000u8, options(nomem, nostack));

        // Write count
        asm!("out 0x42, al", in("al") (COUNT & 0xFF) as u8, options(nomem, nostack));
        asm!("out 0x42, al", in("al") (COUNT >> 8) as u8, options(nomem, nostack));

        // Wait for the PIT to start: reset gate then set gate to trigger countdown
        let gate: u8;
        asm!("in al, 0x61", out("al") gate, options(nomem, nostack));
        asm!("out 0x61, al", in("al") gate & 0xFE, options(nomem, nostack)); // clear gate
        asm!("out 0x61, al", in("al") gate | 0x01, options(nomem, nostack)); // set gate (starts countdown)

        let tsc_start = rdtsc();

        // Wait for PIT output bit (bit 5 of port 0x61) to go high
        loop {
            let status: u8;
            asm!("in al, 0x61", out("al") status, options(nomem, nostack));
            if status & 0x20 != 0 {
                break;
            }
        }

        let tsc_end = rdtsc();
        let delta = tsc_end - tsc_start;

        // Calculate frequency: delta ticks in EXPECTED_US microseconds
        TSC_FREQ = delta * 1_000_000 / EXPECTED_US;
        TSC_BASE = rdtsc();
    }

    crate::serial::print("clock: TSC freq = ");
    crate::serial::print_u64(unsafe { TSC_FREQ });
    crate::serial::println(" Hz");
}

/// Returns nanoseconds since boot.
pub fn nanos_since_boot() -> u64 {
    let now = rdtsc();
    let freq = unsafe { *(&raw const TSC_FREQ) };
    if freq == 0 {
        return 0;
    }
    let elapsed = now - unsafe { *(&raw const TSC_BASE) };
    // Avoid overflow: split computation
    // elapsed / freq gives seconds, remainder gives fractional
    let secs = elapsed / freq;
    let remainder = elapsed % freq;
    secs * 1_000_000_000 + remainder * 1_000_000_000 / freq
}
