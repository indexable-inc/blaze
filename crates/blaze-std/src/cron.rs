//! Pure `#![no_std]` standard 5-field Vixie cron schedule parser and next-occurrence calculator.
//!
//! - Zero heap allocation for [`Schedule`] itself: every field is stored in a compact bitmask
//!   (`minutes: u64`, `hours: u32`, `dom: u32`, `months: u16`, `dow: u8`, plus wildcard flags).
//! - Supports `*`, `*/step`, `N`, `N/step`, `A-B`, `A-B/step`, comma-separated lists,
//!   case-insensitive month names (`JAN`..=`DEC`), case-insensitive weekday names (`SUN`..=`SAT`,
//!   with `7` also mapping to Sunday `0`), and standard Vixie aliases (`@yearly`, `@annually`,
//!   `@monthly`, `@weekly`, `@daily`, `@midnight`, `@hourly`).
//! - Implements standard Vixie DOM/DOW semantics: when both day-of-month and day-of-week are
//!   restricted (neither is `*`), a date matches if EITHER matches; when either is `*`, both must
//!   match.
//! - Zero `unsafe`, zero integer `as` casts, zero `.clone()`, zero panicking slice indexes, and
//!   zero recursion.

use core::alloc::Allocator;
use core::fmt;

use blaze_own::{AllocFailure, Vec};

use crate::time::{Date, DateTime, Time, Weekday};

const MINUTES_MAX: u8 = 59;
const HOURS_MAX: u8 = 23;
const DOM_MIN: u8 = 1;
const DOM_MAX: u8 = 31;
const MONTH_MIN: u8 = 1;
const MONTH_MAX: u8 = 12;
const DOW_MIN: u8 = 0;
const DOW_MAX: u8 = 6;
const DOW_MAX_ACCEPTED: u8 = 7;

/// Maximum span of calendar years searched by [`Schedule::next_after`] (one full 400-year
/// Gregorian cycle).
const MAX_SEARCH_YEARS: i32 = 400;

/// Which of the 5 standard cron fields produced a parse error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CronField {
    /// Minute (`0..=59`).
    Minute,
    /// Hour (`0..=23`).
    Hour,
    /// Day of month (`1..=31`).
    DayOfMonth,
    /// Month (`1..=12`, or `JAN`..=`DEC`).
    Month,
    /// Day of week (`0..=6`, with `7` also accepted as Sunday `0`, or `SUN`..=`SAT`).
    DayOfWeek,
}

impl fmt::Display for CronField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match *self {
            CronField::Minute => "minute",
            CronField::Hour => "hour",
            CronField::DayOfMonth => "day-of-month",
            CronField::Month => "month",
            CronField::DayOfWeek => "day-of-week",
        };
        f.write_str(name)
    }
}

/// Typed error returned when parsing or evaluating a cron [`Schedule`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CronError {
    /// Expression was empty or whitespace-only.
    Empty,
    /// Expression did not contain exactly 5 whitespace-separated fields.
    WrongFieldCount { found: usize },
    /// Field token was not a valid unsigned integer or month/weekday name.
    InvalidNumber,
    /// Step divisor after `/` was missing, zero, or not a valid unsigned integer.
    InvalidStep,
    /// Numeric value was outside the field's allowed bounds.
    OutOfRange { field: CronField, value: u32, min: u8, max: u8 },
    /// Range `start-end` had `start > end`.
    ReversedRange { field: CronField, start: u8, end: u8 },
    /// Unrecognized `@macro` alias.
    UnknownMacro,
    /// No future date satisfies this schedule within the 400-year Gregorian cycle
    /// (for example, `0 0 31 2 *`).
    NoNextOccurrence,
    /// Allocation failed while collecting upcoming occurrences.
    Alloc(AllocFailure),
}

impl fmt::Display for CronError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CronError::Empty => f.write_str("bstd::cron: empty cron expression"),
            CronError::WrongFieldCount { found } => {
                write!(f, "bstd::cron: expected 5 fields, found {found}")
            }
            CronError::InvalidNumber => f.write_str("bstd::cron: invalid numeric or named field value"),
            CronError::InvalidStep => f.write_str("bstd::cron: invalid or zero step value"),
            CronError::OutOfRange { field, value, min, max } => {
                write!(f, "bstd::cron: {field} value {value} out of range ({min}..={max})")
            }
            CronError::ReversedRange { field, start, end } => {
                write!(f, "bstd::cron: {field} range {start}-{end} is reversed")
            }
            CronError::UnknownMacro => f.write_str("bstd::cron: unknown @macro alias"),
            CronError::NoNextOccurrence => {
                f.write_str("bstd::cron: no future occurrence exists in the 400-year calendar cycle")
            }
            CronError::Alloc(failure) => write!(f, "bstd::cron: {failure}"),
        }
    }
}

impl core::error::Error for CronError {}

/// A parsed 5-field Vixie cron schedule stored in compact bitmasks with zero heap allocation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Schedule {
    minutes: u64,
    hours: u32,
    dom: u32,
    months: u16,
    dow: u8,
    dom_wildcard: bool,
    dow_wildcard: bool,
}

impl Schedule {
    /// Parse a 5-field Vixie cron expression or standard `@macro` alias.
    pub fn parse(expr: &str) -> Result<Self, CronError> {
        let trimmed = expr.trim();
        if trimmed.is_empty() {
            return Err(CronError::Empty);
        }

        let expanded = if trimmed.starts_with('@') {
            expand_macro(trimmed)?
        } else {
            trimmed
        };

        let mut fields = [""; 5];
        let mut count = 0usize;
        for part in expanded.split_ascii_whitespace() {
            if let Some(slot) = fields.get_mut(count) {
                *slot = part;
            }
            count += 1;
        }
        if count != 5 {
            return Err(CronError::WrongFieldCount { found: count });
        }

        let [f_min, f_hour, f_dom, f_month, f_dow] = fields;

        let minutes = parse_field_u64(f_min, CronField::Minute, 0, MINUTES_MAX, MINUTES_MAX)?;
        let hours_u64 = parse_field_u64(f_hour, CronField::Hour, 0, HOURS_MAX, HOURS_MAX)?;
        let dom_u64 = parse_field_u64(f_dom, CronField::DayOfMonth, DOM_MIN, DOM_MAX, DOM_MAX)?;
        let months_u64 =
            parse_field_u64(f_month, CronField::Month, MONTH_MIN, MONTH_MAX, MONTH_MAX)?;
        let dow_u64 = parse_field_u64(
            f_dow,
            CronField::DayOfWeek,
            DOW_MIN,
            DOW_MAX,
            DOW_MAX_ACCEPTED,
        )?;

        let hours = u32::try_from(hours_u64).map_err(|_| CronError::InvalidNumber)?;
        let dom = u32::try_from(dom_u64).map_err(|_| CronError::InvalidNumber)?;
        let months = u16::try_from(months_u64).map_err(|_| CronError::InvalidNumber)?;
        let dow = u8::try_from(dow_u64).map_err(|_| CronError::InvalidNumber)?;

        Ok(Self {
            minutes,
            hours,
            dom,
            months,
            dow,
            dom_wildcard: f_dom == "*",
            dow_wildcard: f_dow == "*",
        })
    }

    /// Bitmask of matching minutes (`bit m` set for `m` in `0..=59`).
    #[inline]
    #[must_use]
    pub const fn minutes_mask(&self) -> u64 {
        self.minutes
    }

    /// Bitmask of matching hours (`bit h` set for `h` in `0..=23`).
    #[inline]
    #[must_use]
    pub const fn hours_mask(&self) -> u32 {
        self.hours
    }

    /// Bitmask of matching days of month (`bit d` set for `d` in `1..=31`).
    #[inline]
    #[must_use]
    pub const fn days_of_month_mask(&self) -> u32 {
        self.dom
    }

    /// Bitmask of matching months (`bit m` set for `m` in `1..=12`).
    #[inline]
    #[must_use]
    pub const fn months_mask(&self) -> u16 {
        self.months
    }

    /// Bitmask of matching days of week (`bit w` set for `w` in `0..=6`, where `0` is Sunday).
    #[inline]
    #[must_use]
    pub const fn days_of_week_mask(&self) -> u8 {
        self.dow
    }

    /// Return `true` if `dt`'s calendar components (month, day-of-month / day-of-week, hour,
    /// and minute) satisfy this schedule, ignoring `second` and `nanosecond`.
    #[inline]
    #[must_use]
    pub fn matches_minute(&self, dt: DateTime) -> bool {
        self.matches_minute_planted(dt, false)
    }

    /// Return `true` if `dt` matches this schedule at minute granularity (month, day-of-month /
    /// day-of-week, hour, and minute).
    ///
    /// [`Self::next_after`] always returns a [`DateTime`] with `second == 0` and
    /// `nanosecond == 0` for which `self.matches(next)` is `true`.
    #[inline]
    #[must_use]
    pub fn matches(&self, dt: DateTime) -> bool {
        self.matches_minute(dt)
    }

    /// Find the earliest [`DateTime`] strictly after `dt` (preserving `dt.offset`, with
    /// `second == 0` and `nanosecond == 0`) that satisfies this schedule.
    ///
    /// Searches smartly across matching months, days, hours, and minutes over up to one full
    /// 400-year Gregorian cycle, returning [`CronError::NoNextOccurrence`] if no date matches
    /// (for example `0 0 31 2 *`).
    pub fn next_after(&self, dt: DateTime) -> Result<DateTime, CronError> {
        self.next_after_planted(dt, false)
    }

    /// Collect the next `count` occurrences strictly after `after` into `alloc`.
    pub fn upcoming_in<A: Allocator + Copy>(
        &self,
        after: DateTime,
        count: usize,
        alloc: A,
    ) -> Result<Vec<DateTime, A>, CronError> {
        let mut out = Vec::try_with_capacity_in(count, alloc).map_err(CronError::Alloc)?;
        let mut cursor = after;
        for _ in 0..count {
            let next = self.next_after(cursor)?;
            out.try_push(next).map_err(|refused| CronError::Alloc(refused.why))?;
            cursor = next;
        }
        Ok(out)
    }

    fn matches_date_planted(&self, date: Date, plant_drop_or_rule: bool) -> bool {
        let month_bit = 1u16 << u32::from(date.month());
        if (self.months & month_bit) == 0 {
            return false;
        }
        let dom_match = (self.dom & (1u32 << u32::from(date.day()))) != 0;
        let dow_match = (self.dow & (1u8 << u32::from(weekday_to_cron(date.weekday())))) != 0;
        if !plant_drop_or_rule && !self.dom_wildcard && !self.dow_wildcard {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }

    fn matches_minute_planted(&self, dt: DateTime, plant_drop_or_rule: bool) -> bool {
        if !self.matches_date_planted(dt.date, plant_drop_or_rule) {
            return false;
        }
        let hour_ok = (self.hours & (1u32 << u32::from(dt.time.hour()))) != 0;
        let min_ok = (self.minutes & (1u64 << u32::from(dt.time.minute()))) != 0;
        hour_ok && min_ok
    }

    fn next_after_planted(
        &self,
        dt: DateTime,
        plant_drop_or_rule: bool,
    ) -> Result<DateTime, CronError> {
        let mut year = dt.date.year();
        let mut month = dt.date.month();
        let mut day = dt.date.day();
        let mut hour = dt.time.hour();
        let mut minute = dt.time.minute() + 1;

        if minute >= 60 {
            minute = 0;
            hour += 1;
            if hour >= 24 {
                hour = 0;
                day += 1;
                let dim = Date::days_in_month(year, month);
                if day > dim {
                    day = 1;
                    month += 1;
                    if month > 12 {
                        month = 1;
                        year = year.checked_add(1).ok_or(CronError::NoNextOccurrence)?;
                    }
                }
            }
        }

        let max_year = dt.date.year().saturating_add(MAX_SEARCH_YEARS);

        while year <= max_year {
            // 1. Advance to the next allowed month.
            match next_bit_u16(self.months, month) {
                Some(next_month) => {
                    if next_month != month {
                        month = next_month;
                        day = 1;
                        hour = 0;
                        minute = 0;
                    }
                }
                None => {
                    year = match year.checked_add(1) {
                        Some(y) => y,
                        None => return Err(CronError::NoNextOccurrence),
                    };
                    month = 1;
                    day = 1;
                    hour = 0;
                    minute = 0;
                    continue;
                }
            }

            // 2. Find the earliest matching day in `(year, month)` at or after `day`.
            let dim = Date::days_in_month(year, month);
            let mut found_day = None;
            let mut cand_day = day;
            while cand_day <= dim {
                if let Ok(candidate_date) = Date::new(year, month, cand_day)
                    && self.matches_date_planted(candidate_date, plant_drop_or_rule)
                {
                    found_day = Some(cand_day);
                    break;
                }
                cand_day += 1;
            }

            match found_day {
                Some(next_day) => {
                    if next_day != day {
                        day = next_day;
                        hour = 0;
                        minute = 0;
                    }
                }
                None => {
                    day = 1;
                    hour = 0;
                    minute = 0;
                    if month == 12 {
                        year = match year.checked_add(1) {
                            Some(y) => y,
                            None => return Err(CronError::NoNextOccurrence),
                        };
                        month = 1;
                    } else {
                        month += 1;
                    }
                    continue;
                }
            }

            // 3. Find the earliest matching hour at or after `hour`.
            match next_bit_u32(self.hours, hour) {
                Some(next_hour) => {
                    if next_hour != hour {
                        hour = next_hour;
                        minute = 0;
                    }
                }
                None => {
                    hour = 0;
                    minute = 0;
                    day += 1;
                    if day > dim {
                        day = 1;
                        if month == 12 {
                            year = match year.checked_add(1) {
                                Some(y) => y,
                                None => return Err(CronError::NoNextOccurrence),
                            };
                            month = 1;
                        } else {
                            month += 1;
                        }
                    }
                    continue;
                }
            }

            // 4. Find the earliest matching minute at or after `minute`.
            match next_bit_u64(self.minutes, minute) {
                Some(next_minute) => {
                    if let (Ok(d), Ok(t)) =
                        (Date::new(year, month, day), Time::new(hour, next_minute, 0, 0))
                    {
                        return Ok(DateTime::new(d, t, dt.offset));
                    }
                    return Err(CronError::NoNextOccurrence);
                }
                None => {
                    minute = 0;
                    hour += 1;
                    if hour >= 24 {
                        hour = 0;
                        day += 1;
                        if day > dim {
                            day = 1;
                            if month == 12 {
                                year = match year.checked_add(1) {
                                    Some(y) => y,
                                    None => return Err(CronError::NoNextOccurrence),
                                };
                                month = 1;
                            } else {
                                month += 1;
                            }
                        }
                    }
                }
            }
        }

        Err(CronError::NoNextOccurrence)
    }
}

const fn weekday_to_cron(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Sunday => 0,
        Weekday::Monday => 1,
        Weekday::Tuesday => 2,
        Weekday::Wednesday => 3,
        Weekday::Thursday => 4,
        Weekday::Friday => 5,
        Weekday::Saturday => 6,
    }
}

fn expand_macro(expr: &str) -> Result<&'static str, CronError> {
    if expr.eq_ignore_ascii_case("@yearly") || expr.eq_ignore_ascii_case("@annually") {
        Ok("0 0 1 1 *")
    } else if expr.eq_ignore_ascii_case("@monthly") {
        Ok("0 0 1 * *")
    } else if expr.eq_ignore_ascii_case("@weekly") {
        Ok("0 0 * * 0")
    } else if expr.eq_ignore_ascii_case("@daily") || expr.eq_ignore_ascii_case("@midnight") {
        Ok("0 0 * * *")
    } else if expr.eq_ignore_ascii_case("@hourly") {
        Ok("0 * * * *")
    } else {
        Err(CronError::UnknownMacro)
    }
}

fn parse_field_u64(
    text: &str,
    field: CronField,
    min: u8,
    max: u8,
    max_accepted: u8,
) -> Result<u64, CronError> {
    if text.is_empty() {
        return Err(CronError::InvalidNumber);
    }

    let mut mask = 0u64;
    for item in text.split(',') {
        if item.is_empty() {
            return Err(CronError::InvalidNumber);
        }

        let (range_part, step) = match item.split_once('/') {
            Some((lhs, rhs)) => {
                if rhs.is_empty() || rhs.contains('/') {
                    return Err(CronError::InvalidStep);
                }
                let s = parse_u32_ascii(rhs).ok_or(CronError::InvalidStep)?;
                if s == 0 {
                    return Err(CronError::InvalidStep);
                }
                (lhs, Some(s))
            }
            None => (item, None),
        };

        let (start, end, step_val) = if range_part == "*" {
            (min, max, step.unwrap_or(1))
        } else if let Some((lo_str, hi_str)) = range_part.split_once('-') {
            if hi_str.contains('-') {
                return Err(CronError::InvalidNumber);
            }
            let lo = parse_atom(lo_str, field, min, max_accepted)?;
            let hi = parse_atom(hi_str, field, min, max_accepted)?;
            if lo > hi {
                return Err(CronError::ReversedRange { field, start: lo, end: hi });
            }
            (lo, hi, step.unwrap_or(1))
        } else {
            let single = parse_atom(range_part, field, min, max_accepted)?;
            match step {
                Some(s) => {
                    let norm_start = match field {
                        CronField::DayOfWeek if single == 7 => 0,
                        CronField::Minute
                        | CronField::Hour
                        | CronField::DayOfMonth
                        | CronField::Month
                        | CronField::DayOfWeek => single,
                    };
                    (norm_start, max, s)
                }
                None => (single, single, 1),
            }
        };

        let mut val = u32::from(start);
        let end_u32 = u32::from(end);
        while val <= end_u32 {
            let bit_index = match field {
                CronField::DayOfWeek if val == 7 => 0u32,
                CronField::Minute
                | CronField::Hour
                | CronField::DayOfMonth
                | CronField::Month
                | CronField::DayOfWeek => val,
            };
            if bit_index < 64 {
                mask |= 1u64 << bit_index;
            }
            val = val.saturating_add(step_val);
        }
    }

    if mask == 0 {
        return Err(CronError::InvalidNumber);
    }

    Ok(mask)
}

fn parse_atom(text: &str, field: CronField, min: u8, max_accepted: u8) -> Result<u8, CronError> {
    if text.is_empty() {
        return Err(CronError::InvalidNumber);
    }

    match field {
        CronField::Month => {
            if let Some(m) = parse_month_name(text) {
                return Ok(m);
            }
        }
        CronField::DayOfWeek => {
            if let Some(d) = parse_weekday_name(text) {
                return Ok(d);
            }
        }
        CronField::Minute | CronField::Hour | CronField::DayOfMonth => {}
    }

    let raw = parse_u32_ascii(text).ok_or(CronError::InvalidNumber)?;
    if !(u32::from(min)..=u32::from(max_accepted)).contains(&raw) {
        return Err(CronError::OutOfRange {
            field,
            value: raw,
            min,
            max: max_accepted,
        });
    }
    u8::try_from(raw).map_err(|_| CronError::InvalidNumber)
}

fn parse_u32_ascii(text: &str) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    let mut acc = 0u32;
    for &b in text.as_bytes() {
        if !b.is_ascii_digit() {
            return None;
        }
        acc = acc.checked_mul(10)?.checked_add(u32::from(b - b'0'))?;
    }
    Some(acc)
}

fn parse_month_name(text: &str) -> Option<u8> {
    if text.len() != 3 {
        return None;
    }
    if text.eq_ignore_ascii_case("JAN") {
        Some(1)
    } else if text.eq_ignore_ascii_case("FEB") {
        Some(2)
    } else if text.eq_ignore_ascii_case("MAR") {
        Some(3)
    } else if text.eq_ignore_ascii_case("APR") {
        Some(4)
    } else if text.eq_ignore_ascii_case("MAY") {
        Some(5)
    } else if text.eq_ignore_ascii_case("JUN") {
        Some(6)
    } else if text.eq_ignore_ascii_case("JUL") {
        Some(7)
    } else if text.eq_ignore_ascii_case("AUG") {
        Some(8)
    } else if text.eq_ignore_ascii_case("SEP") {
        Some(9)
    } else if text.eq_ignore_ascii_case("OCT") {
        Some(10)
    } else if text.eq_ignore_ascii_case("NOV") {
        Some(11)
    } else if text.eq_ignore_ascii_case("DEC") {
        Some(12)
    } else {
        None
    }
}

fn parse_weekday_name(text: &str) -> Option<u8> {
    if text.len() != 3 {
        return None;
    }
    if text.eq_ignore_ascii_case("SUN") {
        Some(0)
    } else if text.eq_ignore_ascii_case("MON") {
        Some(1)
    } else if text.eq_ignore_ascii_case("TUE") {
        Some(2)
    } else if text.eq_ignore_ascii_case("WED") {
        Some(3)
    } else if text.eq_ignore_ascii_case("THU") {
        Some(4)
    } else if text.eq_ignore_ascii_case("FRI") {
        Some(5)
    } else if text.eq_ignore_ascii_case("SAT") {
        Some(6)
    } else {
        None
    }
}

fn next_bit_u64(mask: u64, from: u8) -> Option<u8> {
    if from >= 64 {
        return None;
    }
    let shifted = mask >> u32::from(from);
    if shifted == 0 {
        return None;
    }
    u8::try_from(u32::from(from) + shifted.trailing_zeros()).ok()
}

fn next_bit_u32(mask: u32, from: u8) -> Option<u8> {
    if from >= 32 {
        return None;
    }
    let shifted = mask >> u32::from(from);
    if shifted == 0 {
        return None;
    }
    u8::try_from(u32::from(from) + shifted.trailing_zeros()).ok()
}

fn next_bit_u16(mask: u16, from: u8) -> Option<u8> {
    if from >= 16 {
        return None;
    }
    let shifted = mask >> u32::from(from);
    if shifted == 0 {
        return None;
    }
    u8::try_from(u32::from(from) + shifted.trailing_zeros()).ok()
}

#[cfg(test)]
mod tests {
    use core::fmt::Write;

    use blaze_own::{String as OwnString, format_in, testing::heap};
    use blaze_sim::{Broke, Config, Tape};

    use super::{CronError, CronField, Schedule};
    use crate::time::{Date, DateTime, Time, UtcOffset};

    fn dt_utc(year: i32, month: u8, day: u8, hour: u8, minute: u8, second: u8) -> DateTime {
        let d = Date::new(year, month, day).expect("date");
        let t = Time::new(hour, minute, second, 0).expect("time");
        DateTime::utc(d, t)
    }

    #[test]
    fn cron_macros_match_canonical_schedules() {
        assert_eq!(Schedule::parse("@yearly"), Schedule::parse("0 0 1 1 *"));
        assert_eq!(Schedule::parse("@annually"), Schedule::parse("0 0 1 1 *"));
        assert_eq!(Schedule::parse("@ANNUALLY"), Schedule::parse("0 0 1 1 *"));
        assert_eq!(Schedule::parse("@monthly"), Schedule::parse("0 0 1 * *"));
        assert_eq!(Schedule::parse("@weekly"), Schedule::parse("0 0 * * 0"));
        assert_eq!(Schedule::parse("@daily"), Schedule::parse("0 0 * * *"));
        assert_eq!(Schedule::parse("@midnight"), Schedule::parse("0 0 * * *"));
        assert_eq!(Schedule::parse("@hourly"), Schedule::parse("0 * * * *"));
        assert_eq!(Schedule::parse("@reboot"), Err(CronError::UnknownMacro));
        assert_eq!(Schedule::parse("@hourly extra"), Err(CronError::UnknownMacro));
    }

    #[test]
    fn cron_steps_ranges_lists_and_names() {
        let sched = Schedule::parse("5/15 9-17/2 1,15 jan-MAR,dec mon-fri,7").expect("parse");

        // Minutes: 5, 20, 35, 50
        assert_eq!(
            sched.minutes_mask(),
            (1u64 << 5) | (1u64 << 20) | (1u64 << 35) | (1u64 << 50)
        );
        // Hours: 9, 11, 13, 15, 17
        assert_eq!(
            sched.hours_mask(),
            (1u32 << 9) | (1u32 << 11) | (1u32 << 13) | (1u32 << 15) | (1u32 << 17)
        );
        // DOM: 1, 15
        assert_eq!(sched.days_of_month_mask(), (1u32 << 1) | (1u32 << 15));
        // Months: 1, 2, 3, 12
        assert_eq!(
            sched.months_mask(),
            (1u16 << 1) | (1u16 << 2) | (1u16 << 3) | (1u16 << 12)
        );
        // DOW: Sun(0), Mon(1)..=Fri(5)
        assert_eq!(
            sched.days_of_week_mask(),
            (1u8 << 0) | (1u8 << 1) | (1u8 << 2) | (1u8 << 3) | (1u8 << 4) | (1u8 << 5)
        );

        // Range 5-7 covers Fri(5), Sat(6), Sun(0).
        let fri_sun = Schedule::parse("0 0 * * 5-7").expect("5-7");
        assert_eq!(fri_sun.days_of_week_mask(), (1u8 << 5) | (1u8 << 6) | (1u8 << 0));

        // */15 covers 0, 15, 30, 45.
        let qh = Schedule::parse("*/15 * * * *").expect("*/15");
        assert_eq!(
            qh.minutes_mask(),
            (1u64 << 0) | (1u64 << 15) | (1u64 << 30) | (1u64 << 45)
        );
    }

    #[test]
    fn cron_parse_errors() {
        assert_eq!(Schedule::parse(""), Err(CronError::Empty));
        assert_eq!(Schedule::parse("   \t "), Err(CronError::Empty));
        assert_eq!(
            Schedule::parse("* * * *"),
            Err(CronError::WrongFieldCount { found: 4 })
        );
        assert_eq!(
            Schedule::parse("* * * * * *"),
            Err(CronError::WrongFieldCount { found: 6 })
        );
        assert_eq!(Schedule::parse("abc * * * *"), Err(CronError::InvalidNumber));
        assert_eq!(Schedule::parse("1,,2 * * * *"), Err(CronError::InvalidNumber));
        assert_eq!(Schedule::parse("*/0 * * * *"), Err(CronError::InvalidStep));
        assert_eq!(Schedule::parse("*/ * * * *"), Err(CronError::InvalidStep));
        assert_eq!(Schedule::parse("*/2/3 * * * *"), Err(CronError::InvalidStep));
        assert_eq!(
            Schedule::parse("60 * * * *"),
            Err(CronError::OutOfRange {
                field: CronField::Minute,
                value: 60,
                min: 0,
                max: 59,
            })
        );
        assert_eq!(
            Schedule::parse("0 24 * * *"),
            Err(CronError::OutOfRange {
                field: CronField::Hour,
                value: 24,
                min: 0,
                max: 23,
            })
        );
        assert_eq!(
            Schedule::parse("0 0 0 * *"),
            Err(CronError::OutOfRange {
                field: CronField::DayOfMonth,
                value: 0,
                min: 1,
                max: 31,
            })
        );
        assert_eq!(
            Schedule::parse("0 0 * 13 *"),
            Err(CronError::OutOfRange {
                field: CronField::Month,
                value: 13,
                min: 1,
                max: 12,
            })
        );
        assert_eq!(
            Schedule::parse("0 0 * * 8"),
            Err(CronError::OutOfRange {
                field: CronField::DayOfWeek,
                value: 8,
                min: 0,
                max: 7,
            })
        );
        assert_eq!(
            Schedule::parse("10-5 * * * *"),
            Err(CronError::ReversedRange {
                field: CronField::Minute,
                start: 10,
                end: 5,
            })
        );
    }

    #[test]
    fn cron_leap_day_century_and_impossible_dates() {
        let feb29 = Schedule::parse("0 0 29 2 *").expect("feb29");
        let after_2024 = feb29.next_after(dt_utc(2024, 3, 1, 0, 0, 0)).expect("2028");
        assert_eq!(after_2024, dt_utc(2028, 2, 29, 0, 0, 0));

        // 2100 is NOT a leap year; next leap day after 2096-02-29 is 2104-02-29!
        let after_2096 = feb29.next_after(dt_utc(2096, 2, 29, 0, 0, 0)).expect("2104");
        assert_eq!(after_2096, dt_utc(2104, 2, 29, 0, 0, 0));

        // February 31 never occurs.
        let feb31 = Schedule::parse("0 0 31 2 *").expect("feb31");
        assert_eq!(
            feb31.next_after(dt_utc(2026, 1, 1, 0, 0, 0)),
            Err(CronError::NoNextOccurrence)
        );

        // April 31 never occurs.
        let apr31 = Schedule::parse("0 0 31 4,6,9,11 *").expect("apr31");
        assert_eq!(
            apr31.next_after(dt_utc(2026, 1, 1, 0, 0, 0)),
            Err(CronError::NoNextOccurrence)
        );
    }

    #[test]
    fn cron_vixie_dom_dow_or_vs_and_and_upcoming() {
        let h = heap();
        let alloc = h.alloc();

        // Both restricted -> OR: fires on the 15th OR on any Monday.
        // 2026-06-01 is Monday, 2026-06-08 is Monday, 2026-06-15 is Monday,
        // 2026-07-06 is Monday, 2026-07-15 is Wednesday.
        let both = Schedule::parse("30 8 15 * MON").expect("both");
        assert!(both.matches(dt_utc(2026, 7, 6, 8, 30, 0))); // Monday
        assert!(both.matches(dt_utc(2026, 7, 15, 8, 30, 0))); // 15th (Wednesday)
        assert!(!both.matches(dt_utc(2026, 7, 16, 8, 30, 0))); // 16th (Thursday)

        // DOM is wildcard -> AND (only Mondays).
        let only_mon = Schedule::parse("30 8 * * MON").expect("only_mon");
        assert!(only_mon.matches(dt_utc(2026, 7, 6, 8, 30, 0)));
        assert!(!only_mon.matches(dt_utc(2026, 7, 15, 8, 30, 0)));

        // DOW is wildcard -> AND (only the 15th).
        let only_15 = Schedule::parse("30 8 15 * *").expect("only_15");
        assert!(!only_15.matches(dt_utc(2026, 7, 6, 8, 30, 0)));
        assert!(only_15.matches(dt_utc(2026, 7, 15, 8, 30, 0)));

        // Preserve non-UTC offset and advance past non-zero seconds.
        let offset = UtcOffset::from_hm(5, 30).expect("offset");
        let start = DateTime::new(
            Date::new(2026, 7, 1).expect("date"),
            Time::new(8, 30, 12, 500).expect("time"),
            offset,
        );
        let list = both.upcoming_in(start, 3, alloc).expect("upcoming");
        assert_eq!(list.len(), 3);
        assert_eq!(list.first().map(|dt| (dt.date, dt.time, dt.offset)), Some((
            Date::new(2026, 7, 6).expect("d0"),
            Time::new(8, 30, 0, 0).expect("t0"),
            offset,
        )));
        assert_eq!(list.get(1).map(|dt| dt.date), Some(Date::new(2026, 7, 13).expect("d1")));
        assert_eq!(list.get(2).map(|dt| dt.date), Some(Date::new(2026, 7, 15).expect("d2")));
    }

    fn draw_schedule<A: core::alloc::Allocator + Copy>(
        tape: &Tape,
        force_both_dom_dow: bool,
        alloc: A,
    ) -> Schedule {
        let min_step = tape.int_in(1u8..=30, "min_step");
        let min_start = tape.int_in(0u8..=29, "min_start");
        let hour_step = tape.int_in(1u8..=12, "hour_step");
        let hour_start = tape.int_in(0u8..=11, "hour_start");

        let mut expr = OwnString::new_in(alloc);
        write!(&mut expr, "{min_start}/{min_step} {hour_start}/{hour_step} ").expect("fmt");

        let dom_restricted = force_both_dom_dow || tape.coin("dom_restricted");
        let dow_restricted = force_both_dom_dow || tape.coin("dow_restricted");

        if dom_restricted {
            let d1 = tape.int_in(1u8..=28, "dom1");
            let d2 = tape.int_in(1u8..=28, "dom2");
            write!(&mut expr, "{d1},{d2} ").expect("fmt");
        } else {
            expr.push_str("* ");
        }

        // Always allow all months or a dense subset so brute-force minute scan stays fast.
        if tape.coin("month_restricted") {
            let m_step = tape.int_in(1u8..=3, "m_step");
            write!(&mut expr, "*/{m_step} ").expect("fmt");
        } else {
            expr.push_str("* ");
        }

        if dow_restricted {
            let w1 = tape.int_in(0u8..=6, "dow1");
            let w2 = tape.int_in(0u8..=6, "dow2");
            write!(&mut expr, "{w1},{w2}").expect("fmt");
        } else {
            expr.push('*');
        }

        Schedule::parse(expr.as_str()).expect("generated schedule must parse")
    }

    fn draw_datetime(tape: &Tape) -> DateTime {
        let year = tape.int_in(2020i32..=2030, "year");
        let month = tape.int_in(1u8..=12, "month");
        let dim = Date::days_in_month(year, month);
        let day = tape.int_in(1u8..=dim, "day");
        let hour = tape.int_in(0u8..=23, "hour");
        let minute = tape.int_in(0u8..=59, "minute");
        let second = tape.int_in(0u8..=59, "second");
        let date = Date::new(year, month, day).expect("valid date");
        let time = Time::new(hour, minute, second, 0).expect("valid time");
        DateTime::utc(date, time)
    }

    fn verify_next_after<A: core::alloc::Allocator + Copy>(
        sched: Schedule,
        dt: DateTime,
        plant_drop_or_rule: bool,
        alloc: A,
    ) -> Result<(), Broke> {
        let next = sched
            .next_after_planted(dt, plant_drop_or_rule)
            .map_err(|e| Broke::Oracle(format_in!(alloc, "next_after failed: {e}").as_str().into()))?;

        if next.offset != dt.offset {
            return Err(Broke::Oracle("offset was not preserved".into()));
        }
        if next.time.second() != 0 || next.time.nanosecond() != 0 {
            return Err(Broke::Oracle("next_after did not zero seconds/nanoseconds".into()));
        }
        if (next.date, next.time) <= (dt.date, dt.time) {
            return Err(Broke::Oracle("next_after was not strictly after dt".into()));
        }
        // Reference check always uses true Vixie semantics (`plant = false`).
        if !sched.matches(next) {
            return Err(Broke::Oracle(
                format_in!(alloc, "next_after {} does not match schedule", next.date)
                    .as_str()
                    .into(),
            ));
        }

        // Independently verify no minute strictly between `dt` and `next` matches true Vixie
        // semantics by walking every calendar day in `dt.date..=next.date`.
        let start_day = dt.date.to_unix_days();
        let end_day = next.date.to_unix_days();
        let mut day_num = start_day;
        while day_num <= end_day {
            let probe_date = Date::from_unix_days(day_num).expect("probe date");
            if sched.matches_date_planted(probe_date, false) {
                let first_min: u16 = if day_num == start_day {
                    u16::from(dt.time.hour()) * 60 + u16::from(dt.time.minute()) + 1
                } else {
                    0
                };
                let limit_min: u16 = if day_num == end_day {
                    u16::from(next.time.hour()) * 60 + u16::from(next.time.minute())
                } else {
                    24 * 60
                };
                let mut total_min = first_min;
                while total_min < limit_min {
                    let h = u8::try_from(total_min / 60).expect("hour");
                    let m = u8::try_from(total_min % 60).expect("minute");
                    let probe = DateTime::new(
                        probe_date,
                        Time::new(h, m, 0, 0).expect("probe time"),
                        dt.offset,
                    );
                    if sched.matches(probe) {
                        return Err(Broke::Oracle(
                            format_in!(
                                alloc,
                                "skipped earlier match {} {:02}:{:02} before {} {:02}:{:02}",
                                probe.date,
                                h,
                                m,
                                next.date,
                                next.time.hour(),
                                next.time.minute()
                            )
                            .as_str()
                            .into(),
                        ));
                    }
                    total_min += 1;
                }
            }
            day_num += 1;
        }
        Ok(())
    }

    /// The invariant: `next_after` returns the strictly earliest future timestamp matching the schedule under Vixie DOM/DOW semantics.
    #[test]
    fn property_next_after_is_strictly_minimal_match() {
        let h = heap();
        blaze_sim::check(Config::cases(200), |tape| {
            let sched = draw_schedule(tape, false, h.alloc());
            let dt = draw_datetime(tape);
            verify_next_after(sched, dt, false, h.alloc())
        });
    }

    #[test]
    fn the_oracle_catches_skipped_dom_dow_or_rule() {
        let h = heap();
        blaze_sim::control(Config::cases(128), false, true, "", |tape, plant| {
            let sched = draw_schedule(tape, true, h.alloc());
            let dt = draw_datetime(tape);
            verify_next_after(sched, dt, plant, h.alloc())
        });
    }
}
