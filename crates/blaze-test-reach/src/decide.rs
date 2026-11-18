//! Whether a test's result may be reused, and the decision to reuse or rerun it
//! (tests-stage.md, "Reuse" and "Determinism, and what is never reused"), as pure functions.
//!
//! - [`Reusable::of`]: from the grants the test's sandbox was given. A test granted the clock,
//!   the network or the OS's randomness is never reused, by name: its reach cannot say what it
//!   depended on (rule 3: never a stale pass).
//! - [`enter`]: a reusable record whose every cell stands is [`Enter::Reused`]; else the test
//!   reruns, and [`Because`] names the FIRST moved cell in the record's order (what `--why`
//!   prints). [`enter_stored`]: the same from the stored bytes, a missing or unreadable record a
//!   rerun with its reason, never a silent pass or a silent rerun.
//! - [`audit`]: a reusable test run again and compared, out and reach; a difference files it
//!   [`NotReusable::NonDeterministic`].

use core::alloc::Allocator;
use core::fmt;

use crate::record::{Out, RecordError, TestRecord};
use crate::{Digest, Read, Reached};

/// Why a test's result is never reused. Discriminants are the record's reusable byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum NotReusable {
    /// It was granted the wall clock.
    Clock = 1,
    /// It was granted the network.
    Network = 2,
    /// It was granted the OS's randomness, not the seed from its key.
    Randomness = 3,
    /// `blaze test --audit` ran it again and it answered another out or reach.
    NonDeterministic = 4,
    /// One cell answered two values in its run (reach.rs `ReachError::TwoValues`).
    CellMovedDuringRun = 5,
    /// It reached more cells than a record holds (reach.rs `ReachError::Over`).
    ReachOver = 6,
}

impl NotReusable {
    pub const fn from_byte(byte: u8) -> Option<NotReusable> {
        match byte {
            1 => Some(NotReusable::Clock),
            2 => Some(NotReusable::Network),
            3 => Some(NotReusable::Randomness),
            4 => Some(NotReusable::NonDeterministic),
            5 => Some(NotReusable::CellMovedDuringRun),
            6 => Some(NotReusable::ReachOver),
            _ => None,
        }
    }

    pub const fn words(self) -> &'static str {
        match self {
            NotReusable::Clock => "it was granted the clock",
            NotReusable::Network => "it was granted the network",
            NotReusable::Randomness => "it was granted the OS's randomness",
            NotReusable::NonDeterministic => "an audit found it answers differently on equal inputs",
            NotReusable::CellMovedDuringRun => "something it read changed while it ran",
            NotReusable::ReachOver => "it reached more than a record holds",
        }
    }
}

/// Whether a test's result may be reused while its reach stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Reusable {
    Yes,
    No(NotReusable),
}

/// What a test's sandbox was granted beyond the deterministic default (no clock, randomness
/// seeded from the test's key, a fixed hash seed, no network).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grants {
    pub clock: bool,
    pub network: bool,
    pub os_randomness: bool,
}

impl Reusable {
    /// From the grants: the first granted of clock, network, randomness, in that order.
    pub const fn of(grants: Grants) -> Reusable {
        if grants.clock {
            Reusable::No(NotReusable::Clock)
        } else if grants.network {
            Reusable::No(NotReusable::Network)
        } else if grants.os_randomness {
            Reusable::No(NotReusable::Randomness)
        } else {
            Reusable::Yes
        }
    }

    /// The record's byte: 0, or the reason's number.
    pub const fn byte(self) -> u8 {
        match self {
            Reusable::Yes => 0,
            Reusable::No(why) => why as u8,
        }
    }
}

/// Why a test is entered again.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Because {
    /// This cell of its reach moved: the first in the record's order.
    Moved(Read),
    /// Its record says it is never reused.
    NotReusable(NotReusable),
    /// No record of it is stored: it never ran here.
    NoRecord,
    /// Its stored record does not open: rerun, and the note names why (rule 3).
    Unreadable(RecordError),
    /// The record under its name is another test's: a key that misses an input (rule 21).
    OtherTest { found: Digest },
}

impl fmt::Display for Because {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Because::Moved(read) => write!(f, "{read} moved"),
            Because::NotReusable(why) => write!(f, "never reused: {}", why.words()),
            Because::NoRecord => f.write_str("no result of it is kept"),
            Because::Unreadable(error) => write!(f, "its kept result is unreadable, so it reruns: {error}"),
            Because::OtherTest { found } => write!(f, "the result kept under its name is test {found}'s (rule 21)"),
        }
    }
}

/// The decision.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Enter {
    /// Its reach stands: its kept out is its out now.
    Reused,
    Rerun { because: Because },
}

/// Reuse or rerun the test `record` is of: its reach's cells asked of `moved` in the record's
/// order, stopping at the first that moved.
pub fn enter(record: &TestRecord<'_>, mut moved: impl FnMut(&Read) -> bool) -> Enter {
    if let Reusable::No(why) = record.reusable() {
        return Enter::Rerun { because: Because::NotReusable(why) };
    }
    match record.reads().find(|read| moved(read)) {
        Some(read) => Enter::Rerun { because: Because::Moved(read) },
        None => Enter::Reused,
    }
}

/// [`enter`] from the bytes stored under test `test`'s name: none, unreadable or another test's
/// is a rerun with its reason.
pub fn enter_stored(test: Digest, stored: Option<&[u8]>, moved: impl FnMut(&Read) -> bool) -> Enter {
    let Some(bytes) = stored else {
        return Enter::Rerun { because: Because::NoRecord };
    };
    match TestRecord::open(bytes) {
        Err(error) => Enter::Rerun { because: Because::Unreadable(error) },
        Ok(record) if record.test() != test => Enter::Rerun { because: Because::OtherTest { found: record.test() } },
        Ok(record) => enter(&record, moved),
    }
}

/// What an audit's second run showed against the kept result.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Audit {
    Same,
    OutDiffers { was: Out, now: Out },
    /// The reaches differ; the first row where they part, from each side (`None`: that side
    /// ended there).
    ReachDiffers { was: Option<Read>, now: Option<Read> },
}

/// The kept `recorded` against a second run's out and reach. Anything but [`Audit::Same`] files
/// the test [`NotReusable::NonDeterministic`] and is reported by name.
pub fn audit<A: Allocator>(recorded: &TestRecord<'_>, now_out: Out, now: &Reached<A>) -> Audit {
    if recorded.out() != now_out {
        return Audit::OutDiffers { was: recorded.out(), now: now_out };
    }
    if recorded.digest() == now.digest() {
        return Audit::Same;
    }
    let mut was = recorded.reads();
    let mut fresh = now.reads().iter();
    loop {
        match (was.next(), fresh.next()) {
            (Some(a), Some(b)) if a == *b => continue,
            (None, None) => return Audit::Same,
            (a, b) => return Audit::ReachDiffers { was: a, now: b.copied() },
        }
    }
}
