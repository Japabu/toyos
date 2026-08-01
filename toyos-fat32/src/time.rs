/// A wall-clock instant in the form a directory entry stores it.
///
/// This crate never reads a clock. Every call that stamps an entry takes one
/// of these, so the only time that reaches the disk is time the caller chose —
/// the kernel's RTC in production, a fixed value in a test that wants a
/// reproducible image.
///
/// FAT's encoding is lossy in three ways that no wrapper can hide, so they are
/// stated rather than papered over: the epoch is 1980-01-01, seconds are
/// stored in units of two, and the year field is seven bits, so the last
/// representable day is 2107-12-31. [`FatTime::from_unix_secs`] clamps to that
/// range instead of wrapping, because a timestamp that silently reads as 1980
/// is a worse answer than one pinned at the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FatTime {
    date: u16,
    time: u16,
    /// Hundredths within the odd second the two-second `time` field drops.
    /// FAT stores this only for creation time.
    tenths: u8,
}

/// 1980-01-01 00:00:00, the earliest instant FAT can name.
const EPOCH_UNIX_SECS: u64 = 315_532_800;
/// 2107-12-31 23:59:58, the latest.
const MAX_UNIX_SECS: u64 = 4_354_819_198;

const DAY: u64 = 86_400;

impl FatTime {
    /// The FAT epoch. What an entry gets when the caller has no clock — every
    /// field is in range, so nothing downstream has to special-case it.
    pub const EPOCH: FatTime = FatTime {
        date: (0 << 9) | (1 << 5) | 1,
        time: 0,
        tenths: 0,
    };

    /// Clamped to FAT's representable range at both ends; see the type docs.
    pub fn from_unix_secs(secs: u64) -> FatTime {
        let secs = secs.clamp(EPOCH_UNIX_SECS, MAX_UNIX_SECS);
        let days = secs / DAY;
        let rem = secs % DAY;
        let (y, m, d) = civil_from_days(days);

        let hour = rem / 3600;
        let min = (rem % 3600) / 60;
        let sec = rem % 60;

        // `y` is 1980..=2107 by the clamp, so `y - 1980` fits the 7-bit field.
        let date = (((y - 1980) as u16) << 9) | ((m as u16) << 5) | d as u16;
        let time = ((hour as u16) << 11) | ((min as u16) << 5) | (sec / 2) as u16;
        FatTime { date, time, tenths: ((sec % 2) * 100) as u8 }
    }

    /// The inverse, for reading a timestamp back off a volume.
    ///
    /// Total by construction: the field extractions below are masked, and
    /// [`days_from_civil`] is defined for every month and day a mask can
    /// produce — including the month 0 and day 0 that a hostile or
    /// never-initialised entry will have, which it treats as the day before
    /// the first of the following month rather than refusing. A timestamp is
    /// not load-bearing enough to fail a read over.
    pub fn to_unix_secs(&self) -> u64 {
        let year = 1980 + (self.date >> 9) as u64;
        let month = ((self.date >> 5) & 0x0F) as u64;
        let day = (self.date & 0x1F) as u64;
        let hour = (self.time >> 11) as u64;
        let min = ((self.time >> 5) & 0x3F) as u64;
        let sec = (self.time & 0x1F) as u64 * 2;

        let days = days_from_civil(year, month, day);
        days * DAY + hour * 3600 + min * 60 + sec + (self.tenths as u64 / 100)
    }

    pub(crate) fn raw(&self) -> (u16, u16, u8) {
        (self.date, self.time, self.tenths)
    }

    pub(crate) fn from_raw(date: u16, time: u16, tenths: u8) -> FatTime {
        FatTime { date, time, tenths: tenths.min(199) }
    }
}

/// Days since 1970-01-01 to (year, month, day). Hinnant's algorithm, restricted
/// to the non-negative half — `days` here is always derived from a `u64` second
/// count at or after the FAT epoch.
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

/// The inverse. Saturating rather than checked: the only caller feeds it
/// masked bit-fields, and `month` may be 0 or 13..=15 for an entry nobody
/// wrote. Shifting a zero month back a year keeps the arithmetic in range and
/// keeps the function total, which is the property that matters.
fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year.saturating_sub(1) } else { year };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day.saturating_sub(1);
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe).saturating_sub(719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every day FAT can name, both directions. 46,752 of them, so exhaustive
    /// is cheaper than choosing which ones to trust.
    #[test]
    fn every_representable_day_round_trips() {
        let mut secs = EPOCH_UNIX_SECS;
        let mut days = 0;
        while secs <= MAX_UNIX_SECS {
            let t = FatTime::from_unix_secs(secs);
            assert_eq!(t.to_unix_secs(), secs, "at unix {secs}");
            secs += DAY;
            days += 1;
        }
        assert_eq!(days, 46_751);
    }

    #[test]
    fn seconds_round_down_to_two() {
        let base = EPOCH_UNIX_SECS + 12 * 3600 + 34 * 60;
        assert_eq!(FatTime::from_unix_secs(base + 30).to_unix_secs(), base + 30);
        assert_eq!(FatTime::from_unix_secs(base + 31).to_unix_secs(), base + 31);
        // The odd second survives only because `tenths` carries it; the
        // two-second `time` field alone would land on 30.
        let odd = FatTime::from_unix_secs(base + 31);
        assert_eq!(odd.raw().2, 100);
    }

    #[test]
    fn clamps_instead_of_wrapping() {
        assert_eq!(FatTime::from_unix_secs(0), FatTime::EPOCH);
        assert_eq!(FatTime::from_unix_secs(u64::MAX).to_unix_secs(), MAX_UNIX_SECS);
    }

    /// A never-initialised entry is all zeroes, which is month 0 and day 0.
    #[test]
    fn zero_entry_has_a_time() {
        let t = FatTime::from_raw(0, 0, 0);
        assert!(t.to_unix_secs() < EPOCH_UNIX_SECS);
    }

    #[test]
    fn every_bit_pattern_decodes() {
        for date in 0..=u16::MAX {
            for time in [0u16, 0x1234, u16::MAX] {
                let _ = FatTime::from_raw(date, time, 255).to_unix_secs();
            }
        }
    }
}
