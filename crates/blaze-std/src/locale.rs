//! The locale (docs/v2/plan/design/std.md, "The rest of the set", `Locale`): time zone,
//! language and formats. Ambient and different per machine, so a deterministic program takes
//! it from [`crate::Io::locale`] or does without. Once obtained it is plain data: holding one
//! reaches nothing.
//!
//! **Provider.** The simulation only ([`crate::hosted::SimHost::locale`]): blaze-core has no
//! effect that asks the system for its locale.

use crate::name::{Language, ZoneName};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Weekday {
    Monday,
    Tuesday,
    Wednesday,
    Thursday,
    Friday,
    Saturday,
    Sunday,
}

/// A machine's locale, as data.
#[derive(Clone, PartialEq, Eq, Debug)] // clone: plain data and refcounts
pub struct Locale {
    pub zone: ZoneName,
    /// The zone's offset from UTC now, in minutes east.
    pub utc_offset_minutes: i32,
    pub language: Language,
    /// Whether times are written on a 24-hour clock.
    pub hour24: bool,
    /// The decimal separator.
    pub decimal: char,
    /// The day a week starts on.
    pub first_day: Weekday,
}

impl Locale {
    /// The simulation's unless a test says otherwise: UTC, `en`, a 24-hour clock, `.`, weeks
    /// from Monday.
    pub fn simulated() -> Locale {
        Locale {
            zone: ZoneName::new("UTC").expect("UTC is a name"),
            utc_offset_minutes: 0,
            language: Language::new("en").expect("en is a name"),
            hour24: true,
            decimal: '.',
            first_day: Weekday::Monday,
        }
    }
}
