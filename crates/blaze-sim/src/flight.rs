//! Answers in flight: what a simulated host keeps between an effect and its completion, the
//! piece every simulation re-implemented by hand. Which due answer comes next is a choice on the
//! tape, calm being the one asked first, so a calm run is the quiet sequential one and a seeded
//! run reaches every order; a kill drops them all.

#![blaze::sans_io]

use std::alloc::Global;

use crate::tape::Tape;

#[cfg(test)]
mod tests;

/// Virtual nanoseconds.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub struct Nanos(pub u64);

/// A bug a control plants in `Flight::deliver`, for a test's oracle to catch.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// The delivered answer is taken out with `swap_remove`, which moves the last asked into
    /// its place.
    SwapRemove,
}

/// Answers asked for and not yet delivered: what a simulated host keeps between an effect and
/// its completion.
pub struct Flight<T> {
    /// Each answer with the time it is due, in the order asked.
    waiting: Vec<(Nanos, T)>,
    #[cfg(test)]
    plant: Plant,
}

impl<T> Flight<T> {
    pub fn new() -> Flight<T> {
        Flight {
            waiting: Vec::new_in(Global),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    /// `new` with a bug planted, for a control.
    #[cfg(test)]
    pub(crate) fn planted(plant: Plant) -> Flight<T> {
        Flight { waiting: Vec::new_in(Global), plant }
    }

    /// Due at once.
    pub fn ask(&mut self, answer: T) {
        self.ask_at(Nanos(0), answer);
    }

    /// Due at `due`: not delivered before it.
    pub fn ask_at(&mut self, due: Nanos, answer: T) {
        self.waiting.push((due, answer));
    }

    pub fn len(&self) -> usize {
        self.waiting.len()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    /// The earliest time an answer is due.
    pub fn earliest(&self) -> Option<Nanos> {
        self.waiting.iter().map(|&(due, _)| due).min()
    }

    /// One of the answers due by `now`, chosen by the tape: calm is the one asked first. `None`
    /// (and no draw) when none is due.
    pub fn deliver(&mut self, tape: &Tape, now: Nanos) -> Option<T> {
        let due = self.waiting.iter().filter(|&&(at, _)| at <= now).count();
        if due == 0 {
            return None;
        }
        let nth = tape.index(due, "deliver");
        let (place, _) = self
            .waiting
            .iter()
            .enumerate()
            .filter(|&(_, &(at, _))| at <= now)
            .nth(nth)
            .expect("`index` answers below the number of answers due");
        #[cfg(test)]
        {
            if self.plant == Plant::SwapRemove {
                return Some(self.waiting.swap_remove(place).1);
            }
        }
        // `remove`, not `swap_remove`: the others keep the order they were asked in.
        Some(self.waiting.remove(place).1)
    }

    /// A kill: everything in flight is gone. How many were.
    pub fn kill(&mut self) -> usize {
        let gone = self.waiting.len();
        self.waiting.clear();
        gone
    }
}

impl<T> Default for Flight<T> {
    fn default() -> Flight<T> {
        Flight::new()
    }
}
