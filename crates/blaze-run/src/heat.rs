//! [`Heat`]: the one moment each function becomes hot.
//!
//! Nothing counts in a function's code. While a function is in the fast tier its slot word is a
//! count stub ([`crate::Watch::HEAT`]): the stub counts down a per-function counter in a data
//! table beside the words, set to [`Config::bound`] when the stub goes in (a plain decrement: a
//! lost one between two threads only delays the function a call), and at zero calls into the
//! runtime, which queues the slot for the control thread and stores nothing. The control thread
//! hands each such event to [`Heat::reached`]: the first for a slot makes it hot, once for the
//! run, however often the stub fires before its word is gone; the driver then turns the count
//! stub off (`Slots::plain`), so a hot function stops paying for being counted, and gives the
//! slot to the optimizing tier. Relax measured about 5% of cycles in count stubs before removing
//! them at its last tier; here a function pays them for at most `bound` calls.
//!
//! The count is of calls since the stub went in, not decayed by time: a function called `bound`
//! times is worth one background compile whenever the calls came, and that compile never takes
//! the worker kept for a thread's own function.

#![blaze::sans_io]

use blaze_own::{AVec, Alloc, Vec};

use crate::{Plant, SlotId};

/// The heat's parameters.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// The calls at which a function is hot: what each count stub's counter starts at.
    pub bound: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    Config { what: &'static str, value: u64 },
    NoSlot { slot: SlotId, slots: u32 },
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the heat's config: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "a count stub of slot {} fired, past the table's {slots}", slot.0)
            }
        }
    }
}

impl core::error::Error for Error {}

/// Which of one program's slots are hot. See the module's documentation.
pub struct Heat<'a> {
    config: Config,
    /// A flag a slot: given to the tier (once, for the run). One column, scanned by nothing.
    hot: AVec<'a, bool>,
    pub(crate) plant: Plant,
}

impl<'a> Heat<'a> {
    pub fn new(slots: u32, config: Config, alloc: Alloc<'a>) -> Result<Heat<'a>, Error> {
        if config.bound == 0 {
            return Err(Error::Config { what: "a bound of 0 makes every function hot", value: 0 });
        }
        Ok(Heat { config, hot: Vec::from_elem_in(false, slots as usize, alloc), plant: Plant::None })
    }

    /// What a count stub's counter starts at.
    pub fn bound(&self) -> u32 {
        self.config.bound
    }

    /// Whether `slot` is to be counted: a count stub goes in front of its fast code until it is
    /// hot, never after.
    pub fn counted(&self, slot: SlotId) -> bool {
        self.hot.get(slot.index()).is_some_and(|hot| !*hot)
    }

    /// `slot`'s count stub reached zero. Pushes `slot` to `hot` the one time it becomes hot;
    /// a later event of the same slot (a stub that fired again before its word was gone) is
    /// nothing.
    pub fn reached(&mut self, slot: SlotId, hot: &mut AVec<'_, SlotId>) -> Result<(), Error> {
        let slots = self.hot.len() as u32;
        let Some(was) = self.hot.get_mut(slot.index()) else {
            return Err(Error::NoSlot { slot, slots });
        };
        if !*was || self.plant == Plant::HotTwice {
            *was = true;
            hot.push(slot);
        }
        Ok(())
    }
}
