//! Civil (calendar) time from epoch milliseconds, shifted by a UTC offset the caller supplies. The
//! core knows no time zones: the webview reports its offset, and this does the arithmetic, so a
//! date reads as the user's calendar wherever it is rendered.

/// One civil date and time, to the minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

const DAY_MS: i64 = 86_400_000;

impl LocalTime {
    /// The civil time of epoch `ms`, `offset_minutes` east of UTC.
    pub fn from_epoch_ms(ms: i64, offset_minutes: i32) -> Self {
        let local = ms + i64::from(offset_minutes) * 60_000;
        let days = local.div_euclid(DAY_MS);
        let minute_of_day = (local.rem_euclid(DAY_MS) / 60_000) as u32;
        // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year,
            month: month as u32,
            day: day as u32,
            hour: minute_of_day / 60,
            minute: minute_of_day % 60,
        }
    }

    /// `YYYY-MM-DD`.
    pub fn iso_date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }

    /// `YYYY-MM-DD HH:MM`, for people.
    pub fn display(&self) -> String {
        format!("{} {:02}:{:02}", self.iso_date(), self.hour, self.minute)
    }

    /// `YYYY-MM-DD HHMM`, for file names: no colon, which Windows rejects and Finder rewrites.
    pub fn file_stamp(&self) -> String {
        format!("{} {:02}{:02}", self.iso_date(), self.hour, self.minute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-30T23:30:00Z.
    const LATE_EVENING_UTC: i64 = 1_790_811_000_000;

    #[test]
    fn the_offset_moves_the_date_and_the_time() {
        let utc = LocalTime::from_epoch_ms(LATE_EVENING_UTC, 0);
        assert_eq!(utc.display(), "2026-09-30 23:30");
        assert_eq!(
            LocalTime::from_epoch_ms(LATE_EVENING_UTC, 8 * 60).display(),
            "2026-10-01 07:30"
        );
        assert_eq!(
            LocalTime::from_epoch_ms(LATE_EVENING_UTC, -4 * 60).display(),
            "2026-09-30 19:30"
        );
    }

    #[test]
    fn dates_before_the_epoch_and_leap_days_are_right() {
        assert_eq!(LocalTime::from_epoch_ms(0, 0).iso_date(), "1970-01-01");
        assert_eq!(
            LocalTime::from_epoch_ms(-1, 0).display(),
            "1969-12-31 23:59"
        );
        assert_eq!(
            LocalTime::from_epoch_ms(951_782_400_000, 0).iso_date(),
            "2000-02-29"
        );
    }

    #[test]
    fn the_file_stamp_has_no_colon() {
        let t = LocalTime::from_epoch_ms(LATE_EVENING_UTC, 0);
        assert_eq!(t.file_stamp(), "2026-09-30 2330");
    }
}
