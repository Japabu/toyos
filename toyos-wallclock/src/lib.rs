//! The calendar, and the zone offset a userland reader has to recover.
//!
//! Two things live here because two programs need them and the host is where
//! either can be tested: the kernel decodes an RTC into a [`Civil`] and stamps
//! FAT directory entries from it, and `/bin/logd` names one file per boot from
//! the same calendar in the same zone. Before this crate there was one
//! implementation in `kernel/src/clock.rs` that userland could not reach, and
//! the second copy would have been the one whose correctness argument mattered
//! most and whose tests could not run.
//!
//! Nothing here allocates, nothing here is `unsafe`, and nothing here reads a
//! device: it is arithmetic over numbers its callers hand it.
//!
//! # The zone recovery
//!
//! [`resolve`] is the whole of `specs/log-architecture-spec.md`'s wall-clock
//! question, and [`Recovery`] is its honest answer type. The syscall surface
//! gives userland two readings of one instant — `SYS_CLOCK_EPOCH`, which is UTC
//! seconds, and `SYS_CLOCK_REALTIME`, which is local `h:m:s` and no date — so
//! the offset between the zones is a subtraction of seconds-of-day. That
//! subtraction pins the offset **modulo 24 hours**, and the real range of zone
//! offsets is 26 hours wide (UTC−12:00 to UTC+14:00), so a two-hour band is
//! genuinely ambiguous: the same pair of readings is UTC+13 on one day and
//! UTC−11 on the day before. [`Recovery::Ambiguous`] is that band, named rather
//! than guessed, because the two answers differ by a whole day in a file name.
//! `userland/logd/src/wall.rs` is the caller and carries the argument in full.

#![no_std]

use core::fmt;

/// Seconds in a day.
const DAY: u64 = 86_400;

/// The easternmost real zone offset, UTC+14:00 (Line Islands, Kiribati).
pub const MAX_EAST_SECS: u64 = 14 * 3_600;

/// The westernmost, UTC−12:00 (Baker Island), as a positive magnitude.
pub const MAX_WEST_SECS: u64 = 12 * 3_600;

/// A wall-clock instant in the fields a human reads.
///
/// The one calendar in the tree. The RTC decodes its registers into this, the
/// log's file names come out of it, `SYS_CLOCK_REALTIME` answers out of it and
/// FAT's directory stamps are built from it — so there is one conversion
/// between seconds and dates rather than one per caller.
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
    /// **Total, on every field combination, including ones no calendar has.**
    /// [`days_from_civil`] saturates rather than checking, so a month of 0 or
    /// 13..=15 and a day of 0 — which is what a hostile or never-initialised
    /// FAT directory entry decodes to — read as the day before the first of the
    /// following month instead of refusing. That is the property `toyos-fat32`
    /// needs: a timestamp is not load-bearing enough to fail a volume read
    /// over, and its `every_bit_pattern_decodes` asserts it over all 65,536
    /// date encodings. [`Self::is_valid`] is the *other* caller's answer — the
    /// RTC's — and refusing there is what keeps an impossible instant out of
    /// the wall clock.
    pub fn to_unix_secs(&self) -> u64 {
        days_from_civil(self.year, self.month, self.day) * DAY
            + self.hour * 3_600
            + self.min * 60
            + self.sec
    }

    pub fn from_unix_secs(secs: u64) -> Civil {
        let (year, month, day) = civil_from_days(secs / DAY);
        let rem = secs % DAY;
        Civil { year, month, day, hour: rem / 3_600, min: rem % 3_600 / 60, sec: rem % 60 }
    }

    /// `YYYY-MM-DD-HHMMSS`, the stem one boot's log files are named for.
    ///
    /// A `Display` adapter and not a `String`, so this crate allocates nothing
    /// and the kernel can use it from a context that must not.
    pub fn stem(&self) -> Stem {
        Stem(*self)
    }
}

/// [`Civil::stem`]'s rendering. Sortable by name, which is what makes `/log`
/// sort into the order the boots happened in.
pub struct Stem(Civil);

impl fmt::Display for Stem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let t = self.0;
        write!(
            f,
            "{:04}-{:02}-{:02}-{:02}{:02}{:02}",
            t.year, t.month, t.day, t.hour, t.min, t.sec
        )
    }
}

/// The shape [`Stem`] renders, as the character classes a name must match to be
/// one of the log's own files: `d` is a digit and every other byte is itself.
///
/// One declaration, so the writer and the sweeper cannot disagree about what a
/// dated name looks like.
pub const STEM_SHAPE: &[u8] = b"dddd-dd-dd-dddddd";

/// Whether `stem` is exactly what [`Stem`] would have rendered.
pub fn is_stem(stem: &str) -> bool {
    stem.len() == STEM_SHAPE.len()
        && stem.bytes().zip(STEM_SHAPE).all(|(b, want)| match want {
            b'd' => b.is_ascii_digit(),
            c => b == *c,
        })
}

/// What two readings of one instant can be made to say about the zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recovery {
    /// The offset east of UTC, in seconds, and it is the only real one the two
    /// readings admit.
    Offset(i64),
    /// Two real zones a day apart, both consistent with the readings. Seconds
    /// east of UTC, so a caller reporting the refusal can name them.
    Ambiguous { east: i64, west: i64 },
}

/// **There is no third answer, and this is the proof of it.**
///
/// [`Recovery`] used to carry a `NoZone` for "past UTC+14 going east and past
/// UTC−12 going west at once", described as *the middle of the band*. The band
/// has no middle: it **is** the overlap of the two ranges. Their widths sum to
/// 26 hours against a day's 24, so every one of the 86,400 offsets is
/// east-real, or west-real, or both — and the variant stood for a state the
/// arithmetic cannot reach, which every caller then had to write an arm for.
/// `every_offset_of_the_day_is_placed` is the empirical half, over the whole
/// domain rather than over examples; this is the half that holds at compile
/// time and fails the build if either bound is ever narrowed.
const _: () = assert!(MAX_EAST_SECS + MAX_WEST_SECS >= DAY);

/// Recover the local zone's offset from a UTC instant and a local time of day.
///
/// `epoch` is seconds since the Unix epoch (UTC) and `local_secs_of_day` is
/// `h*3600 + m*60 + s` read from the same instant. Both must be readings of one
/// instant — a caller that cannot guarantee that has to bracket them, because
/// the subtraction below has no slop in it to absorb a tick.
pub fn resolve(epoch: u64, local_secs_of_day: u64) -> Recovery {
    let usod = epoch % DAY;
    // Reduced mod `DAY` on both sides already, so one addition keeps the
    // subtraction inside the unsigned domain.
    let off = (local_secs_of_day % DAY + DAY - usod) % DAY;

    let east = off as i64;
    let west = off as i64 - DAY as i64;
    let east_real = off <= MAX_EAST_SECS;
    let west_real = DAY - off <= MAX_WEST_SECS;

    // `(false, false)` is not written because it cannot be produced — the
    // `const` assertion above the enum is the argument — so `(false, _)` is
    // exactly "west and only west" and says so without a fourth arm that no
    // input reaches and no test can cover.
    match (east_real, west_real) {
        (true, true) => Recovery::Ambiguous { east, west },
        (true, false) => Recovery::Offset(east),
        (false, _) => Recovery::Offset(west),
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

#[cfg(test)]
mod tests {
    extern crate alloc;
    use super::*;
    use alloc::format;

    /// The round trip, over the dates a calendar gets wrong: a leap day, a
    /// century that is not a leap year, and the epoch itself.
    #[test]
    fn the_calendar_round_trips_through_the_dates_that_break_one() {
        for secs in [
            0,                 // 1970-01-01
            951_782_400,       // 2000-02-29, the leap day of a leap century
            4_107_542_400,     // 2100-03-01, the day after a February that had 28
            1_786_795_200,     // 2026-08-15 12:00:00
        ] {
            assert_eq!(Civil::from_unix_secs(secs).to_unix_secs(), secs);
        }
        assert_eq!(format!("{}", Civil::from_unix_secs(951_782_400)), "2000-02-29 00:00:00");
        assert_eq!(format!("{}", Civil::from_unix_secs(4_107_542_400).stem()), "2100-03-01-000000");
    }

    /// A name the log writes is a name the log recognises. The two used to be a
    /// format string in one file and a byte-shape in another.
    #[test]
    fn every_stem_this_renders_is_one_it_reads_back() {
        for secs in [0, 951_782_400, 1_786_795_200, 4_107_542_400] {
            let stem = format!("{}", Civil::from_unix_secs(secs).stem());
            assert!(is_stem(&stem), "`{stem}` is not the shape it was rendered as");
        }
        for no in ["2026-08-15-12000", "2026-8-15-120000", "unknown-01", "2026-08-15-12000x"] {
            assert!(!is_stem(no), "`{no}` was accepted as a dated stem");
        }
    }

    #[test]
    fn an_impossible_instant_is_refused_and_a_leap_day_is_not() {
        assert!(Civil { year: 2000, month: 2, day: 29, hour: 0, min: 0, sec: 0 }.is_valid());
        assert!(!Civil { year: 2100, month: 2, day: 29, hour: 0, min: 0, sec: 0 }.is_valid());
        assert!(!Civil { year: 1969, month: 1, day: 1, hour: 0, min: 0, sec: 0 }.is_valid());
        assert!(!Civil { year: 2026, month: 1, day: 1, hour: 0, min: 0, sec: 60 }.is_valid());
    }

    /// The owner's own zone, UTC+2, and the zero-offset machine every image
    /// whose firmware names no zone boots as. Both well inside the unique band.
    #[test]
    fn the_two_zones_this_tree_actually_boots_in_recover_exactly() {
        let epoch = 1_786_795_200; // 2026-08-15 12:00:00 UTC
        assert_eq!(resolve(epoch, 14 * 3_600), Recovery::Offset(2 * 3_600));
        assert_eq!(resolve(epoch, epoch % DAY), Recovery::Offset(0));
    }

    /// **The day boundary, which is the case the recovery has to be argued
    /// over.** A western zone reads a local time of day on the far side of
    /// midnight from UTC's, and the answer is still one zone and one date.
    #[test]
    fn a_zone_across_midnight_from_utc_is_still_unique() {
        // 2026-08-15 02:00:00 UTC is 2026-08-14 21:00:00 at UTC−5.
        let epoch = 1_786_759_200;
        assert_eq!(resolve(epoch, 21 * 3_600), Recovery::Offset(-5 * 3_600));
        let Recovery::Offset(off) = resolve(epoch, 21 * 3_600) else { panic!("not unique") };
        let local = Civil::from_unix_secs(epoch.saturating_add_signed(off));
        assert_eq!(format!("{}", local.stem()), "2026-08-14-210000");
    }

    /// The band that cannot be recovered, by the pair that produces it: one
    /// reading pair, two real zones, two different local **days**.
    #[test]
    fn the_pacific_band_is_two_answers_and_says_so() {
        // 2026-08-15 00:30:00 UTC. Local 13:30 is UTC+13 on the 15th and
        // UTC−11 on the 14th, and nothing in the two readings separates them.
        let epoch = 1_786_753_800;
        assert_eq!(
            resolve(epoch, 13 * 3_600 + 1_800),
            Recovery::Ambiguous { east: 13 * 3_600, west: -11 * 3_600 }
        );
        // And the two candidates really are a day apart, which is what makes
        // guessing between them a mis-named file rather than a rounding error.
        let east = Civil::from_unix_secs(epoch.saturating_add_signed(13 * 3_600));
        let west = Civil::from_unix_secs(epoch - 11 * 3_600);
        assert_eq!(format!("{}", east.stem()), "2026-08-15-133000");
        assert_eq!(format!("{}", west.stem()), "2026-08-14-133000");
    }

    /// Both edges of the band, so the refusal starts where the argument says it
    /// does and not one second either side.
    #[test]
    fn the_band_is_exactly_the_twelve_to_fourteen_hours_the_argument_names() {
        let epoch = 1_786_753_800;
        let at = |off: i64| {
            resolve(epoch, ((epoch % DAY) as i64 + off).rem_euclid(DAY as i64) as u64)
        };
        assert_eq!(at(12 * 3_600 - 1), Recovery::Offset(12 * 3_600 - 1));
        assert!(matches!(at(12 * 3_600), Recovery::Ambiguous { .. }));
        assert!(matches!(at(13 * 3_600), Recovery::Ambiguous { .. }));
        assert!(matches!(at(14 * 3_600), Recovery::Ambiguous { .. }));
        assert_eq!(at(14 * 3_600 + 1), Recovery::Offset(14 * 3_600 + 1 - DAY as i64));
        assert_eq!(at(-12 * 3_600), Recovery::Ambiguous { east: 12 * 3_600, west: -12 * 3_600 });
    }

    /// Every offset a quarter-hour apart across the whole real range is either
    /// recovered exactly or named as one of the two candidates. **A silent
    /// wrong answer is what this refuses to have**, so the assertion is over
    /// the whole domain rather than over three examples.
    #[test]
    fn no_real_offset_is_ever_answered_with_a_different_one() {
        let epoch = 1_786_795_200;
        let mut ambiguous = 0;
        let mut minutes = -12 * 60;
        while minutes <= 14 * 60 {
            let off = minutes as i64 * 60;
            let lsod = ((epoch % DAY) as i64 + off).rem_euclid(DAY as i64) as u64;
            match resolve(epoch, lsod) {
                Recovery::Offset(got) => assert_eq!(got, off, "offset {off} came back as {got}"),
                Recovery::Ambiguous { east, west } => {
                    assert!(off == east || off == west, "offset {off} is neither candidate");
                    ambiguous += 1;
                }
            }
            minutes += 15;
        }
        // The band is two hours wide at quarter-hour steps, counted from both
        // ends of the range: nine offsets east and nine west name each other.
        assert_eq!(ambiguous, 18, "the ambiguous band changed width");
    }

    /// Every second of the day is placed, and the counts are what say there is
    /// no third answer.
    ///
    /// The test above walks the *real* offsets a quarter-hour apart; this one
    /// walks the whole input domain, including the 79,199 seconds-of-day that
    /// no zone sits on. Neither `Offset` nor `Ambiguous` may go missing and
    /// nothing may be left over: 43,200 east-only, 35,999 west-only and 7,201
    /// in the overlap is 86,400 exactly, which is why the fourth case
    /// [`Recovery`] used to have a variant for is a state the arithmetic cannot
    /// produce.
    #[test]
    fn every_offset_of_the_day_is_placed() {
        let epoch = 1_786_795_200;
        let (mut offset, mut ambiguous) = (0u32, 0u32);
        for lsod in 0..DAY {
            match resolve(epoch, lsod) {
                Recovery::Offset(_) => offset += 1,
                Recovery::Ambiguous { east, west } => {
                    assert_eq!(east - west, DAY as i64, "the two candidates are a day apart");
                    ambiguous += 1;
                }
            }
        }
        assert_eq!(offset, 79_199);
        assert_eq!(ambiguous, 7_201);
        assert_eq!(u64::from(offset + ambiguous), DAY);
    }
}
