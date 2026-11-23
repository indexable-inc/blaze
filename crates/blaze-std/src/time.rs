//! Pure `#![no_std]` civil calendar dates, wall times, UTC offsets, Unix epoch conversion,
//! RFC 3339 / ISO 8601 parsing & formatting, and human-readable duration parsing & formatting.
//!
//! - Zero ambient clock access: every conversion is a pure function of its arguments.
//! - Zero heap allocation for parsing and formatting (`fmt::Display`, [`format_duration_into`]).
//! - Zero `unsafe`, zero integer `as` casts, zero panicking slice indexes, zero recursion.

use core::fmt;
use core::time::Duration;

const SECS_PER_MINUTE: i64 = 60;
const SECS_PER_HOUR: i64 = 3_600;
const SECS_PER_DAY: i64 = 86_400;
const MAX_OFFSET_SECS: i32 = 86_399;
const NANOS_PER_SEC_U32: u32 = 1_000_000_000;
const NANOS_PER_SEC_U128: u128 = 1_000_000_000;
const NANOS_PER_SEC_I128: i128 = 1_000_000_000;

/// Day of the week in the proleptic Gregorian calendar.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

impl Weekday {
    /// ISO 8601 weekday number (`1` = Monday ..= `7` = Sunday).
    #[inline]
    #[must_use]
    pub const fn number_from_monday(self) -> u8 {
        match self {
            Weekday::Monday => 1,
            Weekday::Tuesday => 2,
            Weekday::Wednesday => 3,
            Weekday::Thursday => 4,
            Weekday::Friday => 5,
            Weekday::Saturday => 6,
            Weekday::Sunday => 7,
        }
    }

    /// Weekday number starting at Sunday (`1` = Sunday ..= `7` = Saturday).
    #[inline]
    #[must_use]
    pub const fn number_from_sunday(self) -> u8 {
        match self {
            Weekday::Sunday => 1,
            Weekday::Monday => 2,
            Weekday::Tuesday => 3,
            Weekday::Wednesday => 4,
            Weekday::Thursday => 5,
            Weekday::Friday => 6,
            Weekday::Saturday => 7,
        }
    }
}

impl fmt::Display for Weekday {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            Weekday::Monday => "Monday",
            Weekday::Tuesday => "Tuesday",
            Weekday::Wednesday => "Wednesday",
            Weekday::Thursday => "Thursday",
            Weekday::Friday => "Friday",
            Weekday::Saturday => "Saturday",
            Weekday::Sunday => "Sunday",
        };
        f.write_str(name)
    }
}

/// Typed error returned when constructing or parsing civil dates, times, offsets, RFC 3339
/// timestamps, or human durations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TimeError {
    /// Month was outside `1..=12`.
    InvalidMonth { month: u8 },
    /// Day was outside `1..=days_in_month(year, month)`.
    InvalidDay { year: i32, month: u8, day: u8 },
    /// Hour was outside `0..=23`.
    InvalidHour { hour: u8 },
    /// Minute was outside `0..=59`.
    InvalidMinute { minute: u8 },
    /// Second was outside `0..=59`.
    InvalidSecond { second: u8 },
    /// Nanosecond was outside `0..=999_999_999`.
    InvalidNanosecond { nanosecond: u32 },
    /// UTC offset seconds were outside `-86_399..=86_399`.
    InvalidOffset { seconds: i32 },
    /// UTC offset hours and minutes had conflicting signs or out-of-range components.
    InvalidOffsetComponents { hours: i8, minutes: i8 },
    /// Input ended before a complete timestamp or field was parsed.
    UnexpectedEnd { offset: usize },
    /// Expected a specific ASCII delimiter byte at `offset`.
    ExpectedByte { offset: usize, expected: u8, found: u8 },
    /// Expected RFC 3339 date/time separator (`T`, `t`, or ` `) at `offset`.
    ExpectedSeparator { offset: usize, found: u8 },
    /// Expected an ASCII decimal digit at `offset`.
    ExpectedDigit { offset: usize },
    /// Expected RFC 3339 UTC offset (`Z`, `z`, `+HH:MM`, or `-HH:MM`) at `offset`.
    ExpectedOffset { offset: usize },
    /// Fractional seconds or duration fraction exceeded the supported digit limit.
    TooManyFractionalDigits { offset: usize },
    /// Unexpected trailing bytes followed a complete RFC 3339 timestamp.
    TrailingBytes { offset: usize },
    /// Duration input was empty or whitespace-only.
    EmptyDuration,
    /// Unrecognized duration unit starting at `offset`.
    UnknownUnit { offset: usize },
    /// Parsed duration overflowed `core::time::Duration`.
    DurationOverflow,
}

impl fmt::Display for TimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TimeError::InvalidMonth { month } => {
                write!(f, "bstd::time: invalid month {month} (expected 1..=12)")
            }
            TimeError::InvalidDay { year, month, day } => {
                write!(f, "bstd::time: invalid calendar date {year:04}-{month:02}-{day:02}")
            }
            TimeError::InvalidHour { hour } => {
                write!(f, "bstd::time: invalid hour {hour} (expected 0..=23)")
            }
            TimeError::InvalidMinute { minute } => {
                write!(f, "bstd::time: invalid minute {minute} (expected 0..=59)")
            }
            TimeError::InvalidSecond { second } => {
                write!(f, "bstd::time: invalid second {second} (expected 0..=59)")
            }
            TimeError::InvalidNanosecond { nanosecond } => {
                write!(f, "bstd::time: invalid nanosecond {nanosecond} (expected 0..1_000_000_000)")
            }
            TimeError::InvalidOffset { seconds } => {
                write!(
                    f,
                    "bstd::time: invalid UTC offset {seconds}s (expected -{MAX_OFFSET_SECS}..={MAX_OFFSET_SECS})"
                )
            }
            TimeError::InvalidOffsetComponents { hours, minutes } => {
                write!(f, "bstd::time: invalid UTC offset components ({hours}h, {minutes}m)")
            }
            TimeError::UnexpectedEnd { offset } => {
                write!(f, "bstd::time: unexpected end of input at byte offset {offset}")
            }
            TimeError::ExpectedByte { offset, expected, found } => {
                write!(
                    f,
                    "bstd::time: expected byte 0x{expected:02x} at byte offset {offset}, found 0x{found:02x}"
                )
            }
            TimeError::ExpectedSeparator { offset, found } => {
                write!(
                    f,
                    "bstd::time: expected date/time separator (`T`, `t`, or ` `) at byte offset {offset}, found 0x{found:02x}"
                )
            }
            TimeError::ExpectedDigit { offset } => {
                write!(f, "bstd::time: expected ASCII digit at byte offset {offset}")
            }
            TimeError::ExpectedOffset { offset } => {
                write!(
                    f,
                    "bstd::time: expected UTC offset (`Z`, `z`, `+HH:MM`, or `-HH:MM`) at byte offset {offset}"
                )
            }
            TimeError::TooManyFractionalDigits { offset } => {
                write!(f, "bstd::time: too many fractional digits at byte offset {offset}")
            }
            TimeError::TrailingBytes { offset } => {
                write!(f, "bstd::time: unexpected trailing bytes at byte offset {offset}")
            }
            TimeError::EmptyDuration => f.write_str("bstd::time: empty duration string"),
            TimeError::UnknownUnit { offset } => {
                write!(f, "bstd::time: unknown duration unit at byte offset {offset}")
            }
            TimeError::DurationOverflow => {
                f.write_str("bstd::time: duration exceeds core::time::Duration::MAX")
            }
        }
    }
}

impl core::error::Error for TimeError {}

/// A validated proleptic Gregorian calendar date (astronomical year numbering: year `0` is 1 BCE).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Date {
    year: i32,
    month: u8,
    day: u8,
}

impl Date {
    /// Construct a validated calendar date.
    pub const fn new(year: i32, month: u8, day: u8) -> Result<Self, TimeError> {
        if !matches!(month, 1..=12) {
            return Err(TimeError::InvalidMonth { month });
        }
        let max_day = Self::days_in_month(year, month);
        if day == 0 || day > max_day {
            return Err(TimeError::InvalidDay { year, month, day });
        }
        Ok(Self { year, month, day })
    }

    /// Calendar year (astronomical numbering).
    #[inline]
    #[must_use]
    pub const fn year(&self) -> i32 {
        self.year
    }

    /// Calendar month (`1..=12`).
    #[inline]
    #[must_use]
    pub const fn month(&self) -> u8 {
        self.month
    }

    /// Day of the month (`1..=31`).
    #[inline]
    #[must_use]
    pub const fn day(&self) -> u8 {
        self.day
    }

    /// Return `true` if `year` is a leap year in the proleptic Gregorian calendar.
    #[inline]
    #[must_use]
    pub const fn is_leap_year(year: i32) -> bool {
        (year % 4 == 0) && (year % 100 != 0 || year % 400 == 0)
    }

    /// Number of days in `(year, month)`, or `0` if `month` is outside `1..=12`.
    #[inline]
    #[must_use]
    pub const fn days_in_month(year: i32, month: u8) -> u8 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => {
                if Self::is_leap_year(year) {
                    29
                } else {
                    28
                }
            }
            _ => 0,
        }
    }

    /// 1-based ordinal day of the year (`1..=365`, or `1..=366` in leap years).
    #[inline]
    #[must_use]
    pub fn day_of_year(&self) -> u16 {
        let before: u16 = match self.month {
            2 => 31,
            3 => 59,
            4 => 90,
            5 => 120,
            6 => 151,
            7 => 181,
            8 => 212,
            9 => 243,
            10 => 273,
            11 => 304,
            12 => 334,
            _ => 0,
        };
        let leap = u16::from(self.month > 2 && Self::is_leap_year(self.year));
        before + leap + u16::from(self.day)
    }

    /// Day of the week for this date (`1970-01-01` is Thursday).
    #[inline]
    #[must_use]
    pub fn weekday(&self) -> Weekday {
        let days = self.to_unix_days();
        let idx = (days.rem_euclid(7) + 3).rem_euclid(7);
        match idx {
            0 => Weekday::Monday,
            1 => Weekday::Tuesday,
            2 => Weekday::Wednesday,
            3 => Weekday::Thursday,
            4 => Weekday::Friday,
            5 => Weekday::Saturday,
            _ => Weekday::Sunday,
        }
    }

    /// Convert signed days since `1970-01-01` to a proleptic Gregorian [`Date`] using Howard
    /// Hinnant's civil-from-days algorithm. Returns `None` if the resulting year overflows `i32`.
    #[must_use]
    pub fn from_unix_days(unix_days: i64) -> Option<Self> {
        let z = unix_days.checked_add(719_468)?;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year_i64 = if m <= 2 { y + 1 } else { y };
        let year = i32::try_from(year_i64).ok()?;
        let month = u8::try_from(m).ok()?;
        let day = u8::try_from(d).ok()?;
        Some(Self { year, month, day })
    }

    /// Convert this date to signed days since `1970-01-01` using Howard Hinnant's days-from-civil
    /// algorithm.
    #[must_use]
    pub fn to_unix_days(&self) -> i64 {
        self.to_unix_days_planted(false)
    }

    fn to_unix_days_planted(&self, plant_drop_century_rule: bool) -> i64 {
        let mut y = i64::from(self.year);
        let m = i64::from(self.month);
        let d = i64::from(self.day);
        if m <= 2 {
            y -= 1;
        }
        let era = y.div_euclid(400);
        let yoe = y.rem_euclid(400);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let century_correction = if plant_drop_century_rule { 0 } else { yoe / 100 };
        let doe = yoe * 365 + yoe / 4 - century_correction + doy;
        era * 146_097 + doe - 719_468
    }
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.year < 0 {
            let abs = self.year.unsigned_abs();
            write!(f, "-{abs:04}-{:02}-{:02}", self.month, self.day)
        } else if self.year > 9_999 {
            write!(f, "+{}-{:02}-{:02}", self.year, self.month, self.day)
        } else {
            write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
        }
    }
}

/// A validated wall-clock time of day (`00:00:00.000000000` ..= `23:59:59.999999999`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Time {
    hour: u8,
    minute: u8,
    second: u8,
    nanosecond: u32,
}

impl Time {
    /// Midnight (`00:00:00`).
    pub const MIDNIGHT: Self = Self { hour: 0, minute: 0, second: 0, nanosecond: 0 };

    /// Construct a validated wall-clock time.
    pub const fn new(
        hour: u8,
        minute: u8,
        second: u8,
        nanosecond: u32,
    ) -> Result<Self, TimeError> {
        if hour >= 24 {
            return Err(TimeError::InvalidHour { hour });
        }
        if minute >= 60 {
            return Err(TimeError::InvalidMinute { minute });
        }
        if second >= 60 {
            return Err(TimeError::InvalidSecond { second });
        }
        if nanosecond >= NANOS_PER_SEC_U32 {
            return Err(TimeError::InvalidNanosecond { nanosecond });
        }
        Ok(Self { hour, minute, second, nanosecond })
    }

    /// Hour of the day (`0..=23`).
    #[inline]
    #[must_use]
    pub const fn hour(&self) -> u8 {
        self.hour
    }

    /// Minute of the hour (`0..=59`).
    #[inline]
    #[must_use]
    pub const fn minute(&self) -> u8 {
        self.minute
    }

    /// Second of the minute (`0..=59`).
    #[inline]
    #[must_use]
    pub const fn second(&self) -> u8 {
        self.second
    }

    /// Subsecond nanosecond (`0..1_000_000_000`).
    #[inline]
    #[must_use]
    pub const fn nanosecond(&self) -> u32 {
        self.nanosecond
    }

    /// Whole seconds elapsed since midnight (`0..86_400`).
    #[inline]
    #[must_use]
    pub fn seconds_since_midnight(&self) -> u32 {
        u32::from(self.hour) * 3_600 + u32::from(self.minute) * 60 + u32::from(self.second)
    }
}

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}:{:02}", self.hour, self.minute, self.second)?;
        if self.nanosecond != 0 {
            let mut rem = self.nanosecond;
            let mut digits = [b'0'; 9];
            for slot in digits.iter_mut().rev() {
                *slot = match rem % 10 {
                    0 => b'0',
                    1 => b'1',
                    2 => b'2',
                    3 => b'3',
                    4 => b'4',
                    5 => b'5',
                    6 => b'6',
                    7 => b'7',
                    8 => b'8',
                    _ => b'9',
                };
                rem /= 10;
            }
            let mut end = 9usize;
            while end > 1 {
                if digits.get(end - 1) == Some(&b'0') {
                    end -= 1;
                } else {
                    break;
                }
            }
            f.write_str(".")?;
            if let Some(slice) = digits.get(..end)
                && let Ok(text) = core::str::from_utf8(slice)
            {
                f.write_str(text)?;
            }
        }
        Ok(())
    }
}

/// Fixed offset from UTC in seconds east of Greenwich (`-86_399..=86_399`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct UtcOffset {
    seconds: i32,
}

impl UtcOffset {
    /// Zero offset (`Z` / `+00:00`).
    pub const UTC: Self = Self { seconds: 0 };

    /// Construct a [`UtcOffset`] from signed seconds east of UTC (`-86_399..=86_399`).
    pub const fn from_secs(seconds: i32) -> Result<Self, TimeError> {
        if seconds < -MAX_OFFSET_SECS || seconds > MAX_OFFSET_SECS {
            return Err(TimeError::InvalidOffset { seconds });
        }
        Ok(Self { seconds })
    }

    /// Construct a [`UtcOffset`] from signed hours (`-23..=23`) and minutes (`-59..=59`).
    ///
    /// Both non-zero components must share the same sign (for example, `(-5, -30)` for `-05:30`
    /// and `(5, 30)` for `+05:30`); mixed signs are refused.
    pub fn from_hm(hours: i8, minutes: i8) -> Result<Self, TimeError> {
        if !matches!(hours, -23..=23) || !matches!(minutes, -59..=59) {
            return Err(TimeError::InvalidOffsetComponents { hours, minutes });
        }
        if (hours > 0 && minutes < 0) || (hours < 0 && minutes > 0) {
            return Err(TimeError::InvalidOffsetComponents { hours, minutes });
        }
        let total = i32::from(hours) * 3_600 + i32::from(minutes) * 60;
        Self::from_secs(total)
    }

    /// Signed seconds east of UTC (`-86_399..=86_399`).
    #[inline]
    #[must_use]
    pub const fn whole_seconds(&self) -> i32 {
        self.seconds
    }

    /// Return `true` if this offset is UTC (`0` seconds).
    #[inline]
    #[must_use]
    pub const fn is_utc(&self) -> bool {
        self.seconds == 0
    }
}

impl fmt::Display for UtcOffset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.seconds == 0 {
            return f.write_str("Z");
        }
        let sign = if self.seconds < 0 { '-' } else { '+' };
        let abs = self.seconds.unsigned_abs();
        let hours = abs / 3_600;
        let minutes = (abs % 3_600) / 60;
        let secs = abs % 60;
        if secs == 0 {
            write!(f, "{sign}{hours:02}:{minutes:02}")
        } else {
            write!(f, "{sign}{hours:02}:{minutes:02}:{secs:02}")
        }
    }
}

/// A civil date, wall-clock time, and fixed UTC offset.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DateTime {
    pub date: Date,
    pub time: Time,
    pub offset: UtcOffset,
}

impl DateTime {
    /// Construct a [`DateTime`] from a validated [`Date`], [`Time`], and [`UtcOffset`].
    #[inline]
    #[must_use]
    pub const fn new(date: Date, time: Time, offset: UtcOffset) -> Self {
        Self { date, time, offset }
    }

    /// Construct a UTC [`DateTime`] from a validated [`Date`] and [`Time`].
    #[inline]
    #[must_use]
    pub const fn utc(date: Date, time: Time) -> Self {
        Self { date, time, offset: UtcOffset::UTC }
    }

    /// Convert whole Unix seconds since `1970-01-01T00:00:00Z` to a UTC [`DateTime`].
    #[must_use]
    pub fn from_unix_secs(secs: i64) -> Option<Self> {
        let days = secs.div_euclid(SECS_PER_DAY);
        let sod = secs.rem_euclid(SECS_PER_DAY);
        let date = Date::from_unix_days(days)?;
        let hour = u8::try_from(sod / SECS_PER_HOUR).ok()?;
        let rem = sod % SECS_PER_HOUR;
        let minute = u8::try_from(rem / SECS_PER_MINUTE).ok()?;
        let second = u8::try_from(rem % SECS_PER_MINUTE).ok()?;
        let time = Time { hour, minute, second, nanosecond: 0 };
        Some(Self { date, time, offset: UtcOffset::UTC })
    }

    /// Convert Unix nanoseconds since `1970-01-01T00:00:00Z` to a UTC [`DateTime`].
    #[must_use]
    pub fn from_unix_nanos(nanos: i128) -> Option<Self> {
        let secs_i128 = nanos.div_euclid(NANOS_PER_SEC_I128);
        let subsec_i128 = nanos.rem_euclid(NANOS_PER_SEC_I128);
        let secs = i64::try_from(secs_i128).ok()?;
        let nanosecond = u32::try_from(subsec_i128).ok()?;
        let mut dt = Self::from_unix_secs(secs)?;
        dt.time.nanosecond = nanosecond;
        Some(dt)
    }

    /// Convert this [`DateTime`] to whole Unix seconds since `1970-01-01T00:00:00Z`, accounting
    /// for [`Self::offset`].
    #[must_use]
    pub fn to_unix_secs(&self) -> i64 {
        let days = self.date.to_unix_days();
        let sod = i64::from(self.time.seconds_since_midnight());
        let off = i64::from(self.offset.whole_seconds());
        days * SECS_PER_DAY + sod - off
    }

    /// Convert this [`DateTime`] to Unix nanoseconds since `1970-01-01T00:00:00Z`, accounting
    /// for [`Self::offset`].
    #[must_use]
    pub fn to_unix_nanos(&self) -> i128 {
        i128::from(self.to_unix_secs()) * NANOS_PER_SEC_I128 + i128::from(self.time.nanosecond)
    }

    /// Convert this [`DateTime`] to the same instant with [`UtcOffset::UTC`].
    #[must_use]
    pub fn to_utc(&self) -> Option<Self> {
        if self.offset.is_utc() {
            return Some(*self);
        }
        let secs = self.to_unix_secs();
        let mut utc = Self::from_unix_secs(secs)?;
        utc.time.nanosecond = self.time.nanosecond;
        Some(utc)
    }

    /// Strictly parse an RFC 3339 timestamp (`YYYY-MM-DD[T|t| ]HH:MM:SS[.frac](Z|z|±HH:MM)`).
    ///
    /// Accepts up to 9 fractional second digits and validates calendar dates and leap years
    /// strictly. Every syntax error reports the exact byte offset in `input`.
    pub fn parse_rfc3339(input: &str) -> Result<Self, TimeError> {
        let bytes = input.as_bytes();
        let year = parse_year_four(bytes)?;
        expect_byte(bytes, 4, b'-')?;
        let month = parse_u8_two(bytes, 5)?;
        expect_byte(bytes, 7, b'-')?;
        let day = parse_u8_two(bytes, 8)?;
        let date = Date::new(year, month, day)?;

        let sep = *bytes.get(10).ok_or(TimeError::UnexpectedEnd { offset: 10 })?;
        if !matches!(sep, b'T' | b't' | b' ') {
            return Err(TimeError::ExpectedSeparator { offset: 10, found: sep });
        }

        let hour = parse_u8_two(bytes, 11)?;
        expect_byte(bytes, 13, b':')?;
        let minute = parse_u8_two(bytes, 14)?;
        expect_byte(bytes, 16, b':')?;
        let second = parse_u8_two(bytes, 17)?;

        let mut pos = 19usize;
        let mut nanosecond = 0u32;
        if bytes.get(pos) == Some(&b'.') {
            pos += 1;
            let frac_start = pos;
            let mut digits = 0u32;
            while let Some(&b) = bytes.get(pos) {
                if !b.is_ascii_digit() {
                    break;
                }
                if digits >= 9 {
                    return Err(TimeError::TooManyFractionalDigits { offset: pos });
                }
                nanosecond = nanosecond * 10 + u32::from(b - b'0');
                digits += 1;
                pos += 1;
            }
            if pos == frac_start {
                return Err(TimeError::ExpectedDigit { offset: frac_start });
            }
            for _ in digits..9 {
                nanosecond *= 10;
            }
        }

        let time = Time::new(hour, minute, second, nanosecond)?;

        let tz_byte = *bytes.get(pos).ok_or(TimeError::ExpectedOffset { offset: pos })?;
        let offset = match tz_byte {
            b'Z' | b'z' => {
                pos += 1;
                UtcOffset::UTC
            }
            b'+' | b'-' => {
                pos += 1;
                let off_h = parse_u8_two(bytes, pos)?;
                expect_byte(bytes, pos + 2, b':')?;
                let off_m = parse_u8_two(bytes, pos + 3)?;
                if off_h >= 24 {
                    return Err(TimeError::InvalidHour { hour: off_h });
                }
                if off_m >= 60 {
                    return Err(TimeError::InvalidMinute { minute: off_m });
                }
                let mag = i32::from(off_h) * 3_600 + i32::from(off_m) * 60;
                let signed = if tz_byte == b'-' { -mag } else { mag };
                pos += 5;
                UtcOffset::from_secs(signed)?
            }
            _ => return Err(TimeError::ExpectedOffset { offset: pos }),
        };

        if pos != bytes.len() {
            return Err(TimeError::TrailingBytes { offset: pos });
        }

        Ok(Self { date, time, offset })
    }
}

impl fmt::Display for DateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}T{}{}", self.date, self.time, self.offset)
    }
}

fn expect_byte(bytes: &[u8], offset: usize, expected: u8) -> Result<(), TimeError> {
    let found = *bytes.get(offset).ok_or(TimeError::UnexpectedEnd { offset })?;
    if found != expected {
        return Err(TimeError::ExpectedByte { offset, expected, found });
    }
    Ok(())
}

fn parse_digit(bytes: &[u8], offset: usize) -> Result<u8, TimeError> {
    let b = *bytes.get(offset).ok_or(TimeError::UnexpectedEnd { offset })?;
    if !b.is_ascii_digit() {
        return Err(TimeError::ExpectedDigit { offset });
    }
    Ok(b - b'0')
}

fn parse_u8_two(bytes: &[u8], offset: usize) -> Result<u8, TimeError> {
    let hi = parse_digit(bytes, offset)?;
    let lo = parse_digit(bytes, offset + 1)?;
    Ok(hi * 10 + lo)
}

fn parse_year_four(bytes: &[u8]) -> Result<i32, TimeError> {
    let d0 = i32::from(parse_digit(bytes, 0)?);
    let d1 = i32::from(parse_digit(bytes, 1)?);
    let d2 = i32::from(parse_digit(bytes, 2)?);
    let d3 = i32::from(parse_digit(bytes, 3)?);
    Ok(d0 * 1_000 + d1 * 100 + d2 * 10 + d3)
}

/// Parse a human-readable compound duration string into [`core::time::Duration`].
///
/// Supports whitespace-separated or concatenated components such as `"1h 23m 45s"`, `"500ms"`,
/// `"2d 4h"`, `"250us"`, `"10ns"`, and fractional forms such as `"1.5s"`. Supported units are
/// `ns`, `us` / `µs` / `μs`, `ms`, `s` / `sec` / `secs`, `m` / `min` / `mins`, `h` / `hr` /
/// `hrs`, `d` / `day` / `days`, and `w` / `wk` / `wks`.
pub fn parse_duration(input: &str) -> Result<Duration, TimeError> {
    let bytes = input.as_bytes();
    let mut pos = 0usize;
    while let Some(&b) = bytes.get(pos) {
        if b.is_ascii_whitespace() {
            pos += 1;
        } else {
            break;
        }
    }
    if pos == bytes.len() {
        return Err(TimeError::EmptyDuration);
    }

    let mut total_nanos = 0u128;
    while pos < bytes.len() {
        let num_start = pos;
        let mut int_part = 0u128;
        while let Some(&b) = bytes.get(pos) {
            if !b.is_ascii_digit() {
                break;
            }
            int_part = int_part
                .checked_mul(10)
                .and_then(|v| v.checked_add(u128::from(b - b'0')))
                .ok_or(TimeError::DurationOverflow)?;
            pos += 1;
        }
        if pos == num_start {
            return Err(TimeError::ExpectedDigit { offset: num_start });
        }

        let mut frac_num = 0u128;
        let mut frac_scale = 1u128;
        if bytes.get(pos) == Some(&b'.') {
            pos += 1;
            let frac_start = pos;
            let mut frac_digits = 0u32;
            while let Some(&b) = bytes.get(pos) {
                if !b.is_ascii_digit() {
                    break;
                }
                if frac_digits >= 18 {
                    return Err(TimeError::TooManyFractionalDigits { offset: pos });
                }
                frac_num = frac_num * 10 + u128::from(b - b'0');
                frac_scale *= 10;
                frac_digits += 1;
                pos += 1;
            }
            if pos == frac_start {
                return Err(TimeError::ExpectedDigit { offset: frac_start });
            }
        }

        let unit_start = pos;
        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_whitespace() || b.is_ascii_digit() || b == b'.' {
                break;
            }
            pos += 1;
        }
        let unit_slice = bytes.get(unit_start..pos).ok_or(TimeError::UnknownUnit { offset: unit_start })?;
        let unit_str =
            core::str::from_utf8(unit_slice).map_err(|_| TimeError::UnknownUnit { offset: unit_start })?;
        let unit_nanos: u128 = match unit_str {
            "ns" => 1,
            "us" | "µs" | "μs" => 1_000,
            "ms" => 1_000_000,
            "s" | "sec" | "secs" => NANOS_PER_SEC_U128,
            "m" | "min" | "mins" => 60 * NANOS_PER_SEC_U128,
            "h" | "hr" | "hrs" => 3_600 * NANOS_PER_SEC_U128,
            "d" | "day" | "days" => 86_400 * NANOS_PER_SEC_U128,
            "w" | "wk" | "wks" => 604_800 * NANOS_PER_SEC_U128,
            _ => return Err(TimeError::UnknownUnit { offset: unit_start }),
        };

        let whole_nanos = int_part.checked_mul(unit_nanos).ok_or(TimeError::DurationOverflow)?;
        let frac_nanos = if frac_scale > 1 {
            frac_num.checked_mul(unit_nanos).ok_or(TimeError::DurationOverflow)? / frac_scale
        } else {
            0
        };
        total_nanos = total_nanos
            .checked_add(whole_nanos)
            .and_then(|v| v.checked_add(frac_nanos))
            .ok_or(TimeError::DurationOverflow)?;

        while let Some(&b) = bytes.get(pos) {
            if b.is_ascii_whitespace() {
                pos += 1;
            } else {
                break;
            }
        }
    }

    let secs_u128 = total_nanos / NANOS_PER_SEC_U128;
    let sub_nanos_u128 = total_nanos % NANOS_PER_SEC_U128;
    let secs = u64::try_from(secs_u128).map_err(|_| TimeError::DurationOverflow)?;
    let sub_nanos = u32::try_from(sub_nanos_u128).map_err(|_| TimeError::DurationOverflow)?;
    Ok(Duration::new(secs, sub_nanos))
}

fn write_duration_part<W: fmt::Write>(
    out: &mut W,
    wrote_any: &mut bool,
    value: u64,
    unit: &str,
) -> fmt::Result {
    if value == 0 {
        return Ok(());
    }
    if *wrote_any {
        out.write_str(" ")?;
    }
    write!(out, "{value}{unit}")?;
    *wrote_any = true;
    Ok(())
}

/// Format `d` into `out` in a clean, human-readable form that round-trips exactly through
/// [`parse_duration`] for every [`core::time::Duration`] (including `Duration::ZERO` and
/// `Duration::MAX`).
pub fn format_duration_into<W: fmt::Write>(d: Duration, out: &mut W) -> fmt::Result {
    if d.is_zero() {
        return out.write_str("0s");
    }
    let total_secs = d.as_secs();
    let days = total_secs / 86_400;
    let rem_day = total_secs % 86_400;
    let hours = rem_day / 3_600;
    let rem_hour = rem_day % 3_600;
    let mins = rem_hour / 60;
    let secs = rem_hour % 60;

    let sub_nanos = u64::from(d.subsec_nanos());
    let ms = sub_nanos / 1_000_000;
    let rem_ms = sub_nanos % 1_000_000;
    let us = rem_ms / 1_000;
    let ns = rem_ms % 1_000;

    let mut wrote_any = false;
    write_duration_part(out, &mut wrote_any, days, "d")?;
    write_duration_part(out, &mut wrote_any, hours, "h")?;
    write_duration_part(out, &mut wrote_any, mins, "m")?;
    write_duration_part(out, &mut wrote_any, secs, "s")?;
    write_duration_part(out, &mut wrote_any, ms, "ms")?;
    write_duration_part(out, &mut wrote_any, us, "us")?;
    write_duration_part(out, &mut wrote_any, ns, "ns")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use blaze_own::{String as OwnString, format_in, testing::heap};
    use blaze_sim::{Broke, Config};

    use super::*;

    #[test]
    fn leap_years_and_calendar_edges() {
        assert!(!Date::is_leap_year(1900));
        assert!(Date::is_leap_year(2000));
        assert!(Date::is_leap_year(2024));
        assert!(!Date::is_leap_year(2100));
        assert!(Date::is_leap_year(2400));
        assert!(Date::is_leap_year(0));

        assert_eq!(Date::days_in_month(1900, 2), 28);
        assert_eq!(Date::days_in_month(2000, 2), 29);
        assert_eq!(Date::days_in_month(2024, 2), 29);
        assert_eq!(Date::days_in_month(2100, 2), 28);

        assert!(Date::new(1900, 2, 29).is_err());
        assert!(Date::new(2000, 2, 29).is_ok());
        assert!(Date::new(2024, 2, 29).is_ok());
        assert!(Date::new(2100, 2, 29).is_err());

        let epoch = Date::new(1970, 1, 1).expect("epoch");
        assert_eq!(epoch.to_unix_days(), 0);
        assert_eq!(Date::from_unix_days(0), Some(epoch));
        assert_eq!(epoch.weekday(), Weekday::Thursday);
        assert_eq!(epoch.day_of_year(), 1);

        let pre_epoch = Date::new(1969, 12, 31).expect("pre-epoch");
        assert_eq!(pre_epoch.to_unix_days(), -1);
        assert_eq!(Date::from_unix_days(-1), Some(pre_epoch));
        assert_eq!(pre_epoch.weekday(), Weekday::Wednesday);
        assert_eq!(pre_epoch.day_of_year(), 365);

        let y2k_leap = Date::new(2000, 2, 29).expect("2000-02-29");
        let y2k_mar1 = Date::new(2000, 3, 1).expect("2000-03-01");
        assert_eq!(y2k_leap.to_unix_days() + 1, y2k_mar1.to_unix_days());
        assert_eq!(y2k_leap.day_of_year(), 60);
        assert_eq!(y2k_mar1.day_of_year(), 61);
        assert_eq!(Date::from_unix_days(y2k_leap.to_unix_days()), Some(y2k_leap));

        let y1900_feb28 = Date::new(1900, 2, 28).expect("1900-02-28");
        let y1900_mar1 = Date::new(1900, 3, 1).expect("1900-03-01");
        assert_eq!(y1900_feb28.to_unix_days() + 1, y1900_mar1.to_unix_days());
        assert_eq!(Date::from_unix_days(y1900_feb28.to_unix_days()), Some(y1900_feb28));

        let y2100_feb28 = Date::new(2100, 2, 28).expect("2100-02-28");
        let y2100_mar1 = Date::new(2100, 3, 1).expect("2100-03-01");
        assert_eq!(y2100_feb28.to_unix_days() + 1, y2100_mar1.to_unix_days());
        assert_eq!(Date::from_unix_days(y2100_mar1.to_unix_days()), Some(y2100_mar1));

        let far_future = Date::new(9999, 12, 31).expect("9999-12-31");
        assert_eq!(Date::from_unix_days(far_future.to_unix_days()), Some(far_future));
        assert_eq!(far_future.day_of_year(), 365);

        let ancient = Date::new(1, 1, 1).expect("0001-01-01");
        assert_eq!(Date::from_unix_days(ancient.to_unix_days()), Some(ancient));
        assert_eq!(ancient.weekday(), Weekday::Monday);
    }

    #[test]
    fn rfc3339_vectors_and_errors() {
        let h = heap();

        let dt_utc = DateTime::parse_rfc3339("1985-04-12T23:20:50.52Z").expect("rfc3339 utc");
        assert_eq!(dt_utc.date, Date::new(1985, 4, 12).expect("d"));
        assert_eq!(dt_utc.time, Time::new(23, 20, 50, 520_000_000).expect("t"));
        assert_eq!(dt_utc.offset, UtcOffset::UTC);
        assert_eq!(format_in!(h.alloc(), "{dt_utc}").as_str(), "1985-04-12T23:20:50.52Z");

        let dt_off = DateTime::parse_rfc3339("1996-12-19T16:39:57-08:00").expect("rfc3339 -08:00");
        assert_eq!(dt_off.offset, UtcOffset::from_hm(-8, 0).expect("-08:00"));
        assert_eq!(
            dt_off.to_utc(),
            Some(DateTime::utc(
                Date::new(1996, 12, 20).expect("d"),
                Time::new(0, 39, 57, 0).expect("t"),
            ))
        );
        assert_eq!(format_in!(h.alloc(), "{dt_off}").as_str(), "1996-12-19T16:39:57-08:00");

        let dt_space = DateTime::parse_rfc3339("2024-02-29 12:34:56.000000001+05:30").expect("space");
        assert_eq!(dt_space.time.nanosecond(), 1);
        assert_eq!(dt_space.offset, UtcOffset::from_hm(5, 30).expect("+05:30"));
        assert_eq!(
            format_in!(h.alloc(), "{dt_space}").as_str(),
            "2024-02-29T12:34:56.000000001+05:30"
        );

        let dt_lower = DateTime::parse_rfc3339("1970-01-01t00:00:00z").expect("lowercase");
        assert_eq!(dt_lower.to_unix_secs(), 0);
        assert_eq!(dt_lower.to_unix_nanos(), 0);

        let dt_neg_ns = DateTime::from_unix_nanos(-1).expect("neg ns");
        assert_eq!(format_in!(h.alloc(), "{dt_neg_ns}").as_str(), "1969-12-31T23:59:59.999999999Z");
        assert_eq!(dt_neg_ns.to_unix_nanos(), -1);

        assert_eq!(
            DateTime::parse_rfc3339("1900-02-29T00:00:00Z"),
            Err(TimeError::InvalidDay { year: 1900, month: 2, day: 29 })
        );
        assert_eq!(
            DateTime::parse_rfc3339("2024-01-01X00:00:00Z"),
            Err(TimeError::ExpectedSeparator { offset: 10, found: b'X' })
        );
        assert_eq!(
            DateTime::parse_rfc3339("2024-01-01T00:00:00.Z"),
            Err(TimeError::ExpectedDigit { offset: 20 })
        );
        assert_eq!(
            DateTime::parse_rfc3339("2024-01-01T00:00:00.1234567890Z"),
            Err(TimeError::TooManyFractionalDigits { offset: 29 })
        );
        assert_eq!(
            DateTime::parse_rfc3339("2024-01-01T00:00:00Zextra"),
            Err(TimeError::TrailingBytes { offset: 20 })
        );
    }

    #[test]
    fn duration_parsing_and_formatting() {
        let h = heap();
        assert_eq!(parse_duration("0s"), Ok(Duration::ZERO));
        assert_eq!(parse_duration("1h 23m 45s"), Ok(Duration::from_secs(5_025)));
        assert_eq!(parse_duration("500ms"), Ok(Duration::from_millis(500)));
        assert_eq!(parse_duration("2d 4h"), Ok(Duration::from_secs(2 * 86_400 + 4 * 3_600)));
        assert_eq!(parse_duration("250us"), Ok(Duration::from_micros(250)));
        assert_eq!(parse_duration("250µs"), Ok(Duration::from_micros(250)));
        assert_eq!(parse_duration("10ns"), Ok(Duration::from_nanos(10)));
        assert_eq!(parse_duration("1.5s"), Ok(Duration::new(1, 500_000_000)));
        assert_eq!(parse_duration("1h23m45s"), Ok(Duration::from_secs(5_025)));

        let mut buf = OwnString::new_in(h.alloc());
        format_duration_into(Duration::MAX, &mut buf).expect("fmt max");
        assert_eq!(parse_duration(buf.as_str()), Ok(Duration::MAX));

        assert_eq!(parse_duration(""), Err(TimeError::EmptyDuration));
        assert_eq!(parse_duration("   "), Err(TimeError::EmptyDuration));
        assert_eq!(parse_duration("10fortnights"), Err(TimeError::UnknownUnit { offset: 2 }));
        assert_eq!(
            parse_duration("99999999999999999999999999999999999999s"),
            Err(TimeError::DurationOverflow)
        );
        assert_eq!(parse_duration("18446744073709551616s"), Err(TimeError::DurationOverflow));
    }

    /// The invariant: datetime epoch and RFC3339 conversions round-trip accurately including century leap-year rules.
    #[test]
    fn property_datetime_epoch_and_rfc3339_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let year = i32::try_from(tape.int_in(1..=9999, "year")).unwrap_or(1970);
            let month = u8::try_from(tape.int_in(1..=12, "month")).unwrap_or(1);
            let max_day = Date::days_in_month(year, month);
            let day = u8::try_from(tape.int_in(1..=i64::from(max_day), "day")).unwrap_or(1);
            let hour = u8::try_from(tape.int_in(0..=23, "hour")).unwrap_or(0);
            let minute = u8::try_from(tape.int_in(0..=59, "minute")).unwrap_or(0);
            let second = u8::try_from(tape.int_in(0..=59, "second")).unwrap_or(0);
            let has_nanos = tape.coin("has_nanos");
            let nanosecond = if has_nanos {
                u32::try_from(tape.below(1_000_000_000, "nanos")).unwrap_or(0)
            } else {
                0
            };
            let off_mins = i32::try_from(tape.int_in(-1439..=1439, "off_mins")).unwrap_or(0);
            let offset = UtcOffset::from_secs(off_mins * 60).expect("offset");

            let date = Date::new(year, month, day).expect("valid date");
            let time = Time::new(hour, minute, second, nanosecond).expect("valid time");
            let dt = DateTime::new(date, time, offset);

            assert_eq!(Date::from_unix_days(date.to_unix_days()), Some(date));

            let utc = dt.to_utc().expect("to_utc");
            assert_eq!(DateTime::from_unix_nanos(dt.to_unix_nanos()), Some(utc));

            let dt_whole = DateTime::new(date, Time::new(hour, minute, second, 0).expect("t"), offset);
            assert_eq!(DateTime::from_unix_secs(dt_whole.to_unix_secs()), dt_whole.to_utc());

            let formatted = format_in!(h.alloc(), "{dt}");
            assert_eq!(DateTime::parse_rfc3339(formatted.as_str()), Ok(dt));
        });
    }

    #[test]
    fn property_duration_round_trip() {
        let h = heap();
        blaze_sim::check(Config::cases(256), |tape| {
            let secs = tape.next_u64("secs");
            let nanos = u32::try_from(tape.below(1_000_000_000, "nanos")).unwrap_or(0);
            let d = Duration::new(secs, nanos);
            let mut buf = OwnString::new_in(h.alloc());
            format_duration_into(d, &mut buf).expect("format duration");
            assert_eq!(parse_duration(buf.as_str()), Ok(d));
        });
    }

    #[test]
    fn the_oracle_catches_missing_century_leap_rule() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let year = i32::try_from(tape.int_in(1900..=2200, "year")).unwrap_or(2000);
            let month = u8::try_from(tape.int_in(1..=12, "month")).unwrap_or(1);
            let max_day = Date::days_in_month(year, month);
            let day = u8::try_from(tape.int_in(1..=i64::from(max_day), "day")).unwrap_or(1);
            let date = Date::new(year, month, day)
                .map_err(|e| Broke::Oracle(format_in!(h.alloc(), "{e}").as_str().into()))?;
            let unix_days = date.to_unix_days_planted(plant);
            let round = Date::from_unix_days(unix_days);
            if round != Some(date) {
                return Err(Broke::Oracle(
                    format_in!(h.alloc(), "date {date} round-tripped to {round:?}").as_str().into(),
                ));
            }
            Ok(())
        });
    }
}
