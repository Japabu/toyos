//! A civil date, as days since 1970-01-01.
//!
//! Days rather than seconds because that is the resolution of every question
//! asked of it here, and because a comparison against a wall clock at second
//! resolution would flip mid-run.

/// `YYYY-MM-DD`, parsed or refused.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub struct Day(i64);

impl Day {
    pub fn today() -> Day {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a host clock before 1970 is a host to fix, not a date to guess at")
            .as_secs() as i64;
        Day(secs.div_euclid(86_400))
    }

    /// `YYYY-MM-DD`, or `None`.
    ///
    /// The month's own length decides the day, so `2026-02-31` is refused
    /// rather than silently meaning the third of March — which is what a
    /// `1..=31` bound in front of Hinnant's formula does.
    pub fn parse(text: &str) -> Option<Day> {
        let (y, rest) = text.split_once('-')?;
        let (m, d) = rest.split_once('-')?;
        if (y.len(), m.len(), d.len()) != (4, 2, 2) {
            return None;
        }
        let (y, m, d) = (y.parse::<i64>().ok()?, m.parse::<i64>().ok()?, d.parse::<i64>().ok()?);
        let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
        let last = match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => return None,
        };
        if !(1..=last).contains(&d) {
            return None;
        }
        // Hinnant's `days_from_civil`, exact for every date the proleptic
        // Gregorian calendar has.
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        Some(Day(era * 146_097 + doe - 719_468))
    }

    /// `later` minus `self`, in days. Negative when `later` is the earlier one.
    pub fn until(self, later: Day) -> i64 {
        later.0 - self.0
    }

    pub fn plus_days(self, days: i64) -> Day {
        Day(self.0 + days)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_calendar_is_the_one_the_parser_refuses_against() {
        assert!(Day::parse("2026-08-09").is_some());
        assert!(Day::parse("2024-02-29").is_some());
        assert!(Day::parse("2026-02-30").is_none());
        assert!(Day::parse("2026-02-31").is_none());
        assert!(Day::parse("2023-02-29").is_none());
        assert!(Day::parse("2026-04-31").is_none());
        assert!(Day::parse("2026-13-01").is_none());
        assert!(Day::parse("2026-00-01").is_none());
        assert!(Day::parse("2026-08-00").is_none());
        assert!(Day::parse("2026-8-9").is_none());
        assert!(Day::parse("yesterday").is_none());
    }

    #[test]
    fn a_month_of_days_is_a_month_of_arithmetic() {
        let (a, b) = (Day::parse("2026-08-08").unwrap(), Day::parse("2026-09-08").unwrap());
        assert_eq!(a.until(b), 31);
        assert_eq!(b.until(a), -31);
        assert_eq!(a.plus_days(31), b);
        // Across a leap day, which is the arithmetic a hand-rolled month would
        // get wrong.
        let (c, d) = (Day::parse("2024-02-28").unwrap(), Day::parse("2024-03-01").unwrap());
        assert_eq!(c.until(d), 2);
    }
}
