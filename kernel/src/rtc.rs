//! The CMOS real-time clock, decoded once per boot.
//!
//! This is the one piece of hardware in the machine that knows what day it is,
//! and it is also hardware this kernel cannot make behave: it may be absent, it
//! may be wedged, and an unclaimed x86 port answers `0xFF` to every read —
//! whose bit 7, in register A, means "an update is in progress". Waiting for
//! that bit to clear with no bound is a boot that hangs before anything has
//! been printed, on a machine whose screen says nothing yet. Every loop here is
//! bounded.
//!
//! So this module is a decoder for untrusted input and its answer is a
//! [`Result`]. [`clock`] asks once, at boot, and a machine that cannot say what
//! time it is boots anyway with that fact recorded, rather than with a
//! plausible wrong number. Nothing here panics.
//!
//! # What one read is
//!
//! Six registers hold one instant between them, and the clock updates while
//! they are being read. Checking the update flag once and then reading them one
//! at a time leaves a window: an update landing after the check gives a time up
//! to a minute wrong, and on New Year's Eve a year wrong. [`read`] takes the
//! whole set twice and accepts only two that agree, which is the only evidence
//! available that the set describes a single instant.

use core::fmt;

use crate::arch::cpu;
use crate::clock;
use toyos_wallclock::Civil;

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0A;
const STATUS_B: u8 = 0x0B;

/// Register A bit 7: the clock registers are mid-update and must not be read.
const UPDATE_IN_PROGRESS: u8 = 1 << 7;
/// Register B bit 1: hours run 0..=23 rather than 1..=12 with a PM flag.
const HOUR_24: u8 = 1 << 1;
/// Register B bit 2: the registers hold binary rather than BCD.
const BINARY: u8 = 1 << 2;
/// Bit 7 of the hours register, in 12-hour mode only.
const PM: u8 = 1 << 7;

/// How long [`UPDATE_IN_PROGRESS`] may stay set before this module gives up on
/// the clock.
///
/// The MC146818 raises the flag 244 µs ahead of an update and clears it when
/// the update ends, at most 1984 µs later, so a working RTC clears it inside
/// 2.3 ms. This is fifty times that, because the two errors are not symmetric:
/// erring long costs a boot delay nobody can perceive on a machine whose clock
/// is broken anyway, and erring short costs the wall clock on a machine that is
/// merely slow.
///
/// What the caller sees when it is hit is [`RtcFault::Updating`], which reaches
/// the boot log by name and leaves this boot with no wall clock — a state every
/// consumer of [`clock`] can represent.
const MAX_UIP_NANOS: u64 = 100_000_000;

/// How many times [`read`] takes the whole register set hoping for two in a row
/// that agree.
///
/// Four, so three disagreements in a row are needed before it gives up. One
/// update can land between any two reads; two in a row means the reads are
/// slower than the clock, and three means the registers are not describing an
/// instant at all.
const MAX_READ_ATTEMPTS: u32 = 4;

/// Why this machine did not say what time it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtcFault {
    /// The update flag never cleared inside [`MAX_UIP_NANOS`]. What an absent
    /// RTC looks like: an unclaimed port reads `0xFF`, and bit 7 of that is the
    /// flag.
    Updating,
    /// No two consecutive reads of the register set agreed, in
    /// [`MAX_READ_ATTEMPTS`] tries.
    Unstable,
    /// The registers agreed on something that is not a date — a BCD digit above
    /// nine, a thirteenth month, an hour no clock has.
    NotADate,
}

impl fmt::Display for RtcFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Updating => write!(
                f,
                "its update flag never cleared in {} ms, which is what an absent one looks like",
                MAX_UIP_NANOS / 1_000_000
            ),
            Self::Unstable => write!(
                f,
                "no two of {MAX_READ_ATTEMPTS} reads agreed, so its registers never described one instant"
            ),
            Self::NotADate => write!(f, "its registers hold something that is not a date"),
        }
    }
}

/// What time the machine says it is, in whatever zone it keeps its clock in.
///
/// `century_reg` is the CMOS index the FADT named, from
/// [`acpi::rtc_century_register`](crate::drivers::acpi::rtc_century_register).
/// `None` there is firmware saying the machine has no such register, and the
/// year then comes from two digits and the assumption [`decode`] states.
pub fn read(century_reg: Option<u8>) -> Result<Civil, RtcFault> {
    // The bound below is a duration, so the monotonic clock has to be running
    // already. Init order is the kernel's own business, which makes this
    // fail-fast rather than one of the faults above.
    assert!(clock::calibrated(), "rtc::read before the monotonic clock was calibrated");

    let mut previous = read_registers(century_reg)?;
    for _ in 1..MAX_READ_ATTEMPTS {
        let current = read_registers(century_reg)?;
        if current == previous {
            return decode(current);
        }
        previous = current;
    }
    Err(RtcFault::Unstable)
}

/// One read of every register the instant is spread across.
///
/// `PartialEq` is the reason this is a struct: two of these being equal is the
/// whole argument that either of them describes a real instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Registers {
    sec: u8,
    min: u8,
    hour: u8,
    day: u8,
    month: u8,
    year: u8,
    /// `None` when the FADT named no century register, which is a different
    /// fact from a century register that holds zero.
    century: Option<u8>,
    status_b: u8,
}

fn read_registers(century_reg: Option<u8>) -> Result<Registers, RtcFault> {
    wait_for_update()?;
    Ok(Registers {
        sec: cmos_read(SECONDS),
        min: cmos_read(MINUTES),
        hour: cmos_read(HOURS),
        day: cmos_read(DAY),
        month: cmos_read(MONTH),
        year: cmos_read(YEAR),
        century: century_reg.map(century_read),
        status_b: cmos_read(STATUS_B),
    })
}

/// The century register the FADT named, at the index it named.
///
/// The actuator here answers the *next* century instead of what the register
/// holds, which is the only way to see the register's contents reach the year.
/// QEMU maintains the clock registers from `-rtc base=` and leaves CMOS 0x32
/// alone at whatever firmware last wrote — measured: a guest booted at 2101
/// reads century 20 and year 01, and reports 2001 — so the host can set every
/// digit of the date except this one.
fn century_read(reg: u8) -> u8 {
    if crate::actuator::rtc_century_next() { 0x21 } else { cmos_read(reg) }
}

fn wait_for_update() -> Result<(), RtcFault> {
    let deadline = clock::nanos_since_boot() + MAX_UIP_NANOS;
    while cmos_read(STATUS_A) & UPDATE_IN_PROGRESS != 0 {
        if clock::nanos_since_boot() >= deadline {
            return Err(RtcFault::Updating);
        }
        core::hint::spin_loop();
    }
    Ok(())
}

/// Register B declares the format of the other six, so both encodings and both
/// hour conventions are read out of it rather than assumed. This is the
/// boundary the hardware's format is decoded at, and nothing inward of it knows
/// that a clock can count in BCD.
fn decode(r: Registers) -> Result<Civil, RtcFault> {
    let binary = r.status_b & BINARY != 0;
    let field = |raw: u8| {
        if binary { Some(raw) } else { bcd_to_bin(raw) }.ok_or(RtcFault::NotADate)
    };

    let sec = field(r.sec)?;
    let min = field(r.min)?;
    let day = field(r.day)?;
    let month = field(r.month)?;
    let year_lo = field(r.year)?;

    let hour = if r.status_b & HOUR_24 != 0 {
        field(r.hour)?
    } else {
        let afternoon = r.hour & PM != 0;
        let twelve = field(r.hour & !PM)?;
        if !(1..=12).contains(&twelve) {
            return Err(RtcFault::NotADate);
        }
        twelve % 12 + if afternoon { 12 } else { 0 }
    };

    let year = match r.century {
        Some(raw) => {
            let century = field(raw)?;
            // 1900 at the earliest. A machine reporting a century before that
            // is reporting a register nobody maintains, and refusing is the
            // answer rather than dropping back to the two-digit assumption
            // below: firmware named this register, so what is in it is the
            // machine's own claim about what year it is.
            if !(19..=99).contains(&century) {
                return Err(RtcFault::NotADate);
            }
            century as u64 * 100 + year_lo as u64
        }
        // Two digits and nothing to widen them with. 2000 rather than the
        // pivot older systems use, because this kernel boots UEFI machines and
        // there are none of those from the 1900s.
        None => 2000 + year_lo as u64,
    };

    let civil = Civil {
        year,
        month: month as u64,
        day: day as u64,
        hour: hour as u64,
        min: min as u64,
        sec: sec as u64,
    };
    if !civil.is_valid() {
        return Err(RtcFault::NotADate);
    }
    Ok(civil)
}

/// `None` for a nibble above nine, which is not a digit and so not a time.
fn bcd_to_bin(bcd: u8) -> Option<u8> {
    let (tens, ones) = (bcd >> 4, bcd & 0x0F);
    (tens <= 9 && ones <= 9).then_some(tens * 10 + ones)
}

fn port_read(reg: u8) -> u8 {
    cpu::outb(CMOS_ADDR, reg);
    cpu::inb(CMOS_DATA)
}

/// What the *hardware* answers, and the one place either clock actuator
/// replaces it. Everything downstream — the decoder, the matched-pair rule,
/// [`wait_for_update`]'s bound — is shipped code reading whatever comes back.
///
/// `rtc-dead` is an RTC that is not there: every register reads `0xFF`, which
/// is what an unclaimed x86 port answers and what an absent or wedged clock
/// looks like from software. Bit 7 of that is [`UPDATE_IN_PROGRESS`], so it is
/// also the only way to reach [`wait_for_update`]'s bound.
///
/// `rtc-unstable` is a clock whose registers change under the reader: the
/// seconds register answers a different valid BCD value every read, so no two
/// reads of the set can agree. That is [`read`]'s matched-pair requirement seen
/// from the failing side — what a torn read looks like when it never stops.
///
/// Neither can be staged from the host: QEMU has no switch that removes or
/// wedges the mc146818, and its RTC presents the guest a coherent register set
/// at every instant.
fn cmos_read(reg: u8) -> u8 {
    if crate::actuator::rtc_dead() {
        return 0xFF;
    }
    if crate::actuator::rtc_unstable() && reg == SECONDS {
        use core::sync::atomic::{AtomicU8, Ordering::Relaxed};
        static TICK: AtomicU8 = AtomicU8::new(0);
        // 0x01..=0x09, so every answer is a valid BCD second and what this
        // stages is `Unstable` rather than `NotADate`.
        return TICK.fetch_add(1, Relaxed) % 9 + 1;
    }
    port_read(reg)
}
