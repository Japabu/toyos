//! The machine's clocks: monotonic since boot, and the wall clock anchored to
//! it once.
//!
//! The monotonic half calibrates the TSC against the HPET at boot and reads the
//! TSC afterwards. The wall-clock half reads the CMOS RTC **exactly once**, in
//! [`init_wall`], and answers every later question as that reading plus
//! [`nanos_since_boot`].
//!
//! Once and not per call, for two reasons that were both live. `SYS_CLOCK_REALTIME`
//! went to the CMOS on every call, so a process asking the time in a loop drove
//! a port-I/O handshake per iteration and could block on the update flag for as
//! long as a second. And each FAT volume read the RTC privately when it mounted,
//! so a machine with two of them had two answers to what time it was and no
//! rule about which won.
//!
//! Once also means the answer can be *absent*: an RTC that never replies is a
//! boot with no wall clock, [`local_secs`] and [`utc_secs`] say so in their
//! return type, and every consumer decides what to do about it. Nothing here
//! invents 1970.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

use crate::mm::paging::CachePolicy;
use crate::arch::cpu;

// HPET register offsets
const HPET_CAP: u64 = 0x000;
const HPET_CFG: u64 = 0x010;
const HPET_COUNTER: u64 = 0x0F0;

static TSC_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_PERIOD_FS: AtomicU64 = AtomicU64::new(0);

pub fn init(hpet_base: u64) {
    let hpet = crate::mm::paging::map_mmio(hpet_base, 0x1000, CachePolicy::DeferToMtrr);

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

/// Whether [`nanos_since_boot`] measures anything yet. Before [`init`] the TSC
/// period is zero and every reading is zero, so a caller that waits on a
/// deadline would wait forever.
pub fn calibrated() -> bool {
    TSC_PERIOD_FS.load(Relaxed) != 0
}

/// Returns nanoseconds since boot. Lock-free, no MMIO, **and it cannot panic**.
///
/// [`TSC_BOOT`] is one global, stored by the BSP during [`init`]. A CPU whose
/// TSC reads below it therefore subtracts to a negative number — and with
/// overflow-checks on, which every guest build has, that is a panic. It was a
/// latent one until `log::emit` began reading the clock inside its publication
/// bracket: there it would fire with `IF` clear and a reservation taken and not
/// yet committed, and the panic handler's own `log!` would reenter the same
/// shard. **No panic site may be inside that bracket.**
///
/// Saturating, and the direction is the argument rather than the taste. A
/// trailing CPU's records stamp zero, so they read as the oldest thing the
/// machine has: `read.rs`'s merge sorts them last and its descent stops at
/// them, which loses that CPU's lines from a bracketed report and costs nothing
/// else. Wrapping would stamp them 584 years in the future, where they would
/// sort ahead of every real record for the life of the boot and take the top of
/// every panel with them. Losing a CPU's lines is recoverable; being lied to
/// about which line is newest is not.
///
/// It stays an assumption that this does not happen: §2.1 of
/// `specs/log-architecture-spec.md` rests cross-CPU ordering on an invariant,
/// firmware-synchronised TSC, and
/// `specs/issues/kernel/ap-tsc-trail-is-assumed-and-never-checked.md` is the
/// entry for the fact that nothing measures it.
pub fn nanos_since_boot() -> u64 {
    let delta = cpu::rdtsc().saturating_sub(TSC_BOOT.load(Relaxed));
    let period_fs = TSC_PERIOD_FS.load(Relaxed);
    ((delta as u128 * period_fs as u128) / 1_000_000) as u64
}

/// A [`cpu::rdtsc`] value `nanos` in the future, for a wait whose loop may not
/// call anything.
///
/// [`nanos_since_boot`]'s 128-bit divide is `__udivti3`, an out-of-line call in
/// `compiler_builtins`, so a spin that reads the clock every iteration spends a
/// large fraction of its time at an address that is not in the spinning
/// function. That is invisible to everything except an instrument that samples
/// the instruction pointer — and `sched::dump`'s NMI probe is exactly one:
/// `dump_nmi_probe` reported `u128_div_rem+0x99` for a CPU that was in the deaf
/// window all along, on `main`'s CI run `31280877870` and twice more.
pub fn tsc_deadline(nanos: u64) -> u64 {
    let period_fs = TSC_PERIOD_FS.load(Relaxed);
    let ticks = (nanos as u128 * 1_000_000) / period_fs.max(1) as u128;
    cpu::rdtsc().saturating_add(ticks as u64)
}

/// Unix seconds, in the machine's own zone, at `nanos_since_boot() == 0`.
static BOOT_LOCAL_SECS: AtomicU64 = AtomicU64::new(0);
/// Seconds to add to the machine's own zone to get UTC — firmware's
/// `EFI_TIME::TimeZone`, whose relation is `Localtime = UTC - TimeZone`.
static UTC_OFFSET_SECS: AtomicI64 = AtomicI64::new(0);
/// Whether the two above mean anything. Not a sentinel in either: zero is a
/// real instant and a real offset.
static WALL_KNOWN: AtomicBool = AtomicBool::new(false);

/// Read the RTC, once, and anchor the wall clock to the monotonic one.
///
/// Call after [`init`] and before anything that stamps a file or serves a
/// clock syscall. A machine whose RTC will not answer logs why and boots with
/// no wall clock, which is a state and not a failure.
pub fn init_wall(century_reg: Option<u8>, utc_offset_minutes: Option<i32>) {
    // A machine whose firmware names a zone. OVMF ships
    // `EFI_UNSPECIFIED_TIMEZONE` and nothing in QEMU sets the UEFI variable
    // that would change it, so every emulated boot takes the "assume UTC"
    // branch and the arithmetic that separates local time from UTC is
    // otherwise never run. Two hours east, which is the owner's own zone and
    // the sign that matters: UEFI's relation is `Localtime = UTC - TimeZone`,
    // so UTC+2 reports -120 and UTC is *behind* what the RTC reads.
    let utc_offset_minutes =
        if crate::actuator::rtc_zone_east() { Some(-120) } else { utc_offset_minutes };

    let civil = match crate::rtc::read(century_reg) {
        Ok(civil) => civil,
        Err(fault) => {
            log!("clock: this machine will not say what time it is — {fault}");
            return;
        }
    };

    let local = civil.to_unix_secs();
    let offset_secs = utc_offset_minutes.unwrap_or(0) as i64 * 60;
    BOOT_LOCAL_SECS.store(local.saturating_sub(nanos_since_boot() / 1_000_000_000), Relaxed);
    UTC_OFFSET_SECS.store(offset_secs, Relaxed);
    WALL_KNOWN.store(true, Release);

    match utc_offset_minutes {
        Some(minutes) => log!("clock: the RTC reads {civil}, {minutes} minutes from UTC by firmware"),
        None => log!("clock: the RTC reads {civil}; firmware named no zone, so it is taken as UTC"),
    }
}

/// What time it is in the zone the machine keeps its clock in — what FAT
/// entries are stamped with, since FAT stores local time by specification, and
/// what this boot's log file is named for.
///
/// `None` is a machine that never said what time it is.
pub fn local_secs() -> Option<u64> {
    WALL_KNOWN
        .load(Acquire)
        .then(|| BOOT_LOCAL_SECS.load(Relaxed) + nanos_since_boot() / 1_000_000_000)
}

/// The same instant as seconds since the Unix epoch, which is UTC by
/// definition. What `SYS_CLOCK_EPOCH` serves and what `SystemTime` is built on.
pub fn utc_secs() -> Option<u64> {
    let local = local_secs()?;
    Some(local.saturating_add_signed(UTC_OFFSET_SECS.load(Relaxed)))
}

/// A wall-clock instant in the fields a human reads.
///
/// The kernel's one calendar. The RTC decodes its registers into this, the log
/// file is named from it, and `SYS_CLOCK_REALTIME` answers out of it — so there
/// is one conversion between seconds and dates in the kernel rather than one
/// per caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Civil {
    pub year: u64,
    pub month: u64,
    pub day: u64,
    pub hour: u64,
    pub min: u64,
    pub sec: u64,
}

impl fmt::Display for Civil {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.min, self.sec
        )
    }
}

impl Civil {
    /// Whether these fields name a day that exists, at a time that exists.
    ///
    /// The Unix epoch is the floor because everything downstream counts
    /// unsigned seconds from it. A leap second lands on 60 and is rejected: the
    /// RTC does not report one, and a clock that does is not one this kernel
    /// understands.
    pub fn is_valid(&self) -> bool {
        (1970..=9999).contains(&self.year)
            && (1..=12).contains(&self.month)
            && (1..=days_in_month(self.year, self.month)).contains(&self.day)
            && self.hour < 24
            && self.min < 60
            && self.sec < 60
    }

    /// Seconds from the Unix epoch to this instant, reading it in the same zone
    /// the epoch is in.
    ///
    /// Saturating on an invalid instant rather than refusing, because the one
    /// caller that can produce one is [`Self::is_valid`]'s own check.
    pub fn to_unix_secs(&self) -> u64 {
        days_from_civil(self.year, self.month, self.day) * 86_400
            + self.hour * 3_600
            + self.min * 60
            + self.sec
    }

    pub fn from_unix_secs(secs: u64) -> Civil {
        let (year, month, day) = civil_from_days(secs / 86_400);
        let rem = secs % 86_400;
        Civil { year, month, day, hour: rem / 3_600, min: rem % 3_600 / 60, sec: rem % 60 }
    }
}

fn is_leap(year: u64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    const LENGTHS: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    match month {
        2 if is_leap(year) => 29,
        1..=12 => LENGTHS[month as usize - 1],
        _ => 0,
    }
}

/// Days from 1970-01-01 to this date. Hinnant's algorithm, restricted to the
/// non-negative half — the epoch is [`Civil::is_valid`]'s floor.
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year.saturating_sub(1) } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day.saturating_sub(1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).saturating_sub(719_468)
}

/// The inverse.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}
