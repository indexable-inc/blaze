//! A calendar day, `YYYY-MM-DD`: an advisory's date, an acceptance's expiry and today, which
//! the driver reads from its clock once and hands in. No time of day and no zone: a day is
//! the unit every rule here is stated in.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::fmt;

/// One day of the proleptic Gregorian calendar, years 1 to 9999.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Date {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

/// Why a text is no [`Date`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NotADate {
    pub text: String,
}

impl fmt::Display for NotADate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`{}` is not a day written YYYY-MM-DD", self.text)
    }
}

fn leap(year: u16) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

impl Date {
    /// `YYYY-MM-DD`, exactly: four, two and two digits, a real day.
    pub fn parse(text: &str) -> Result<Date, NotADate> {
        let refused = || NotADate { text: text.to_owned() };
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(refused());
        }
        let digits = |range: std::ops::Range<usize>| -> Option<u16> {
            let part = text.get(range)?;
            part.bytes().all(|it| it.is_ascii_digit()).then(|| part.parse().ok()).flatten()
        };
        let (Some(year), Some(month), Some(day)) = (digits(0..4), digits(5..7), digits(8..10)) else {
            return Err(refused());
        };
        let (month, day) = (month as u8, day as u8);
        if year == 0 || !(1..=12).contains(&month) || day == 0 || day > days_in(year, month) {
            return Err(refused());
        }
        Ok(Date { year, month, day })
    }

    /// The day after this one.
    pub fn next(self) -> Date {
        if self.day < days_in(self.year, self.month) {
            return Date { day: self.day + 1, ..self };
        }
        if self.month < 12 {
            return Date { month: self.month + 1, day: 1, ..self };
        }
        Date { year: self.year + 1, month: 1, day: 1 }
    }

    /// Days since 0001-01-01: what an age is counted in.
    pub fn ordinal(self) -> i64 {
        let y = i64::from(self.year) - 1;
        let before_year = y * 365 + y / 4 - y / 100 + y / 400;
        let before_month: i64 = (1..self.month).map(|m| i64::from(days_in(self.year, m))).sum();
        before_year + before_month + i64::from(self.day) - 1
    }

    /// The day `days` after 1970-01-01: what a driver makes of its clock's seconds / 86400.
    pub fn from_unix_days(days: u64) -> Date {
        // Bounded: one step a year, and the years of 64 bits of days end long past 9999,
        // where this stops.
        let mut date = Date { year: 1970, month: 1, day: 1 };
        let mut left = days;
        while date.year < 9999 {
            let year_len = if leap(date.year) { 366 } else { 365 };
            if left < year_len {
                break;
            }
            left -= year_len;
            date.year += 1;
        }
        while left > 0 && date.year <= 9999 {
            let month_left = u64::from(days_in(date.year, date.month) - date.day) + 1;
            if left < month_left {
                date.day += left as u8;
                break;
            }
            left -= month_left;
            date = Date { day: days_in(date.year, date.month), ..date }.next();
        }
        date
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}
