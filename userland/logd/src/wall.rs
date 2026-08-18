//! What time it is, in the zone the machine keeps its clock in — recovered
//! from two syscalls rather than asked for with a third.
//!
//! # The problem
//!
//! `/log`'s file names are local timestamps and have been since the kernel
//! wrote them — identical naming, so a
//! stick from before this change and one from after sort together. The kernel
//! had `clock::local_secs()`. Userland has two calls and neither is it:
//!
//! - `clock_epoch` — seconds since the Unix epoch, which is **UTC** by
//!   definition. A full instant, in the wrong zone.
//! - `clock_realtime` — the **local** time of day as `h:m:s`. The right zone,
//!   and no date.
//!
//! Neither alone names a local *date*, and a file called `2026-08-15-…` is one.
//!
//! # The recovery, and exactly how far it is sound
//!
//! The offset is the difference of the two readings' seconds-of-day, which
//! `toyos_wallclock::resolve` computes and this module brackets:
//!
//! ```text
//! usod = epoch mod 86400                  // UTC seconds of day
//! lsod = h*3600 + m*60 + s                // local seconds of day
//! off  = (lsod - usod) mod 86400          // in [0, 86400)
//! ```
//!
//! **The two readings must be of one instant, so [`local_now`] brackets them**:
//! epoch, then local, then epoch again, retrying while the two epoch reads
//! differ. A second cannot have ticked inside a bracket that closed on the value
//! it opened with, so the subtraction is exact and carries no slop — the
//! quarter-hour granularity that real zone offsets have is corroboration here
//! rather than something the arithmetic leans on.
//!
//! **What `off` pins is the offset modulo 24 hours, and that is not the whole
//! answer.** `lsod` is a time of day, so the two candidates differ by exactly
//! 86,400 seconds, and whether both are real is the question:
//!
//! - `off ∈ [0h, 12h)` — the other candidate, `off − 24h`, is west of UTC−12:00
//!   (Baker Island), which is no zone. **Unique.**
//! - `off ∈ (14h, 24h)` — `off` itself is east of UTC+14:00 (Line Islands),
//!   which is no zone, so `off − 24h ∈ (−10h, 0)` is the only one left.
//!   **Unique.**
//! - `off ∈ [12h, 14h]` — **both are real.** `+12:00` (New Zealand) against
//!   `−12:00`; `+13:00` (Tonga) against `−11:00` (Samoa); `+14:00` (Kiribati)
//!   against `−10:00` (Hawaii). Nothing in the two readings separates them, and
//!   the two answers are the same time of day on **different days**.
//!
//! The band exists because the real range of offsets is 26 hours wide and a
//! time of day is only 24. That is the day-boundary case stated exactly: a
//! machine at UTC+13 reading local 13:30 on 2026-08-15 and one at UTC−11
//! reading local 13:30 on 2026-08-14 produce the *same* pair of syscall
//! answers, and a file named from a guess between them is a day wrong.
//!
//! # What this program does about it
//!
//! It refuses. The band answers [`Wall::Ambiguous`] with both candidates named,
//! and `main` falls back to the `unknown-NN.log` name the format already has
//! for a boot that cannot be placed in time — because a file that claims a date
//! it cannot establish is worse than one that says it has none, which is the
//! rule `UNDATED_STEM` was written under in the first place.
//!
//! **Not shipped as a permanent answer.** The clean fix is one field:
//! `SYS_CLOCK_REALTIME` answering a full civil date rather than `h:m:s`, or a
//! call handing back the offset the kernel already holds in `UTC_OFFSET_SECS`.
//! An ABI change is the owner's, so this recovers what is recoverable and
//! refuses the rest rather than guessing on its own authority.
//!
//! The arithmetic and its whole-domain gate are `toyos-wallclock/`, which is a
//! host-workspace member: the argument above is the one thing here that a test
//! inside a guest could not check cheaply, and it is checked on the host at
//! every real offset a quarter-hour apart across the entire range.

use toyos::system::{clock_epoch, clock_realtime};
use toyos_wallclock::{resolve, Recovery};

/// How many times [`local_now`] re-reads the clock to close its bracket.
///
/// A bracket fails only when a second ticks between the first epoch read and
/// the second, which is a window two syscalls wide; three attempts is already
/// past what any machine that is running needs, and the bound is here so a
/// machine whose clock is doing something inexplicable cannot spin.
const ATTEMPTS: u32 = 3;

/// What the machine can be persuaded to say about the wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wall {
    /// Local seconds since the Unix epoch, and the offset they were recovered
    /// with.
    Local { secs: u64, offset_secs: i64 },
    /// This machine never said what time it is — no RTC, one that is wedged, or
    /// one answering with something that is not a date. It is this for the whole
    /// of such a boot rather than intermittently, because the kernel reads the
    /// clock once.
    Unknown,
    /// Two readings naming two real zones a day apart, in seconds east of UTC,
    /// so the line that reports the refusal can print both.
    Ambiguous { east: i64, west: i64 },
}

/// Local seconds since the Unix epoch, now.
pub fn local_now() -> Wall {
    for _ in 0..ATTEMPTS {
        let Some(before) = clock_epoch() else { return Wall::Unknown };
        let Some(local) = clock_realtime() else { return Wall::Unknown };
        let Some(after) = clock_epoch() else { return Wall::Unknown };
        if before != after {
            // A second ticked inside the bracket, so the two readings are not of
            // one instant. Take it again rather than subtract them anyway.
            continue;
        }
        let lsod =
            local.hours as u64 * 3_600 + local.minutes as u64 * 60 + local.seconds as u64;
        // **Two answers and not three.** There used to be a `Recovery::NoZone`
        // arm here for "past UTC+14 going east and past UTC−12 going west at
        // once", called the middle of the band — and the band has no middle,
        // because the band *is* where the two ranges overlap. Their widths sum
        // to 26 hours against a day's 24, so every second of the day is placed
        // by one of the two arms below; `toyos_wallclock`'s `const` assertion
        // is the proof and its whole-domain test is the measurement.
        return match resolve(after, lsod) {
            Recovery::Offset(offset_secs) => {
                Wall::Local { secs: after.saturating_add_signed(offset_secs), offset_secs }
            }
            Recovery::Ambiguous { east, west } => Wall::Ambiguous { east, west },
        };
    }
    // Three brackets in a row that would not close is a clock this program
    // cannot read, which has the same answer as one that will not speak.
    Wall::Unknown
}
