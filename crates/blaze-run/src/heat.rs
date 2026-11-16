//! [`Heat`]: how hot each function is, and the one moment it becomes hot.
//!
//! Fast code charges its slot's counter in its prologue (a plain add to a `u32` in the table's
//! mapping beside the words; a lost increment between two threads only cools a function a
//! little). The control thread samples the counters on its clock and charges the differences
//! here. A function whose heat reaches [`Config::bound`] is hot: it is given to the optimizing
//! tier once, and never again in the run, however hot it stays. Heat decays: every
//! [`Config::period`] of the clock, each count is shifted right by [`Config::shift`], so a
//! function that ran hot once at the start and never again does not stay hot by age alone.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::AllocatorClone;

use crate::{Nanos, Plant, SlotId};

/// The heat's parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The count at which a function is hot.
    pub bound: u32,
    /// The decay's period on the driver's clock.
    pub period: Nanos,
    /// How far each count is shifted right each period: 1 halves it.
    pub shift: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Config { what: &'static str, value: u64 },
    NoSlot { slot: SlotId, slots: u32 },
    /// A time before the last one given.
    Backwards { now: Nanos, last: Nanos },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the heat's config: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "heat charged to slot {}, past the table's {slots}", slot.0)
            }
            Error::Backwards { now, last } => {
                write!(f, "the heat's clock went from {} ns back to {} ns", last.0, now.0)
            }
        }
    }
}

impl core::error::Error for Error {}

/// The counts of one program's slots. See the module's documentation.
pub struct Heat<A: AllocatorClone> {
    config: Config,
    counts: Vec<u32, A>,
    /// Whether a slot was given to the tier: once, for the run.
    queued: Vec<bool, A>,
    /// The start of the period the counts are in.
    since: Nanos,
    pub(crate) plant: Plant,
}

impl<A: AllocatorClone> Heat<A> {
    pub fn new(slots: u32, config: Config, start: Nanos, alloc: A) -> Result<Heat<A>, Error> {
        if config.bound == 0 {
            return Err(Error::Config { what: "a bound of 0 makes every function hot", value: 0 });
        }
        if config.period.0 == 0 {
            return Err(Error::Config { what: "a decay period of 0 ns", value: 0 });
        }
        if config.shift == 0 || config.shift > 31 {
            return Err(Error::Config {
                what: "a decay shift outside 1 to 31",
                value: u64::from(config.shift),
            });
        }
        let mut counts = Vec::new_in(alloc.clone());
        counts.resize(slots as usize, 0);
        let mut queued = Vec::new_in(alloc);
        queued.resize(slots as usize, false);
        Ok(Heat { config, counts, queued, since: start, plant: Plant::None })
    }

    /// The decay up to `now`: one shift for each whole period since the last.
    pub fn tick(&mut self, now: Nanos) -> Result<(), Error> {
        if now < self.since {
            return Err(Error::Backwards { now, last: self.since });
        }
        let periods = (now.0 - self.since.0) / self.config.period.0;
        if periods == 0 {
            return Ok(());
        }
        self.since = Nanos(self.since.0 + periods * self.config.period.0);
        let shift = periods.saturating_mul(u64::from(self.config.shift));
        for count in &mut self.counts {
            *count = if shift >= 32 { 0 } else { *count >> shift };
        }
        Ok(())
    }

    /// `by` more calls of `slot` in this period. Pushes `slot` to `hot` the one time it becomes
    /// hot.
    pub fn charge(&mut self, slot: SlotId, by: u32, hot: &mut Vec<SlotId, A>) -> Result<(), Error> {
        let slots = self.counts.len() as u32;
        let Some(count) = self.counts.get_mut(slot.index()) else {
            return Err(Error::NoSlot { slot, slots });
        };
        *count = count.saturating_add(by);
        let queued = &mut self.queued[slot.index()];
        if *count >= self.config.bound && (!*queued || self.plant == Plant::HotTwice) {
            *queued = true;
            hot.push(slot);
        }
        Ok(())
    }

    pub fn count(&self, slot: SlotId) -> Option<u32> {
        self.counts.get(slot.index()).copied()
    }
}
