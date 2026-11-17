//! The program's personality routine (docs: port27/tempo-run/UNWIND.md, 3.3; piece P3).
//!
//! A program built `panic = "unwind"` has exactly one `#[lang = "eh_personality"]` fn: std's
//! (the pinned library's `std/src/sys/personality/gcc.rs`, `rust_eh_personality`), or a
//! `#![no_core]` program's own. It is in no code record: every record's unwind entry names it
//! through the CIE's personality pointer, which is written at placement (blaze-code eh.rs
//! `Placed::personality`), so the run's parent tells the program which slot it is
//! (blaze-launch's run wire, `Down::Personality`) and the program's run mode writes that slot's
//! entry thunk into every entry it makes. The walk takes the fn as a root: nothing calls it; the
//! system's unwinder does, through the pointer (crate::seam's [`FnLang::ALL`] holds it, and
//! front2/reach.rs queues every fn of that list the program's world declares).
//!
//! A program built `panic = "abort"` lands nowhere (blaze-build `Panic::Abort` builds no
//! landing block), so no entry names a personality and none is asked for.

use blaze_build::Panic;
use blaze_typeck::ItemKey;

use crate::front::FnLang;
use crate::seam::FrontEnd;

/// Why the program has no personality routine to name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PersonalityError {
    /// Built `panic = "unwind"`, and the program's crates declare no fn under
    /// `#[lang = "eh_personality"]` (or two different ones under its names: the front end
    /// answers neither).
    Missing,
}

impl PersonalityError {
    /// The rule that refused, for a finding's class.
    pub const fn class(self) -> &'static str {
        match self {
            PersonalityError::Missing => "no personality routine",
        }
    }
}

impl core::fmt::Display for PersonalityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PersonalityError::Missing => f.write_str(
                "the program is built `panic = \"unwind\"` and declares no `#[lang = \"eh_personality\"]` fn: no unwind entry of its code can name a personality routine",
            ),
        }
    }
}

impl core::error::Error for PersonalityError {}

/// The fn every unwind entry of `program`'s code names as its personality routine: `None` for a
/// program built `panic = "abort"` (nothing lands, so nothing names one); for one built
/// `panic = "unwind"`, its `#[lang = "eh_personality"]` fn, or [`PersonalityError::Missing`].
pub fn personality<P: FrontEnd>(program: &P, panic: blaze_build::Panic) -> Result<Option<ItemKey>, PersonalityError> {
    match panic {
        Panic::Abort => Ok(None),
        Panic::Unwind => match program.fn_lang(FnLang::EhPersonality) {
            Some(key) => Ok(Some(key)),
            None => Err(PersonalityError::Missing),
        },
    }
}
