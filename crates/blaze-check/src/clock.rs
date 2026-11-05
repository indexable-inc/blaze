//! Vector clocks: one `u32` per model thread, in a fixed array so a clock is copied, joined and
//! compared without a branch per thread or an allocation.

/// The most threads a model may have, its main thread included (loom's `MAX_THREADS` is 5).
pub const MAX_THREADS: usize = 8;

/// A model thread's index in its execution.
pub(crate) type Tid = usize;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Hash)]
pub(crate) struct Clock(pub(crate) [u32; MAX_THREADS]);

impl Clock {
    pub(crate) const ZERO: Clock = Clock([0; MAX_THREADS]);

    #[inline]
    pub(crate) fn get(&self, thread: Tid) -> u32 {
        self.0[thread]
    }

    #[inline]
    pub(crate) fn tick(&mut self, thread: Tid) -> u32 {
        self.0[thread] += 1;
        self.0[thread]
    }

    #[inline]
    pub(crate) fn set(&mut self, thread: Tid, at: u32) {
        self.0[thread] = at;
    }

    #[inline]
    pub(crate) fn join(&mut self, other: &Clock) {
        for (mine, theirs) in self.0.iter_mut().zip(other.0) {
            *mine = (*mine).max(theirs);
        }
    }

    /// Everything `self` saw, `other` saw too: `self` happens before (or is) `other`. Every lane
    /// is compared, with no early exit, so it is one vector compare (DPOR asks it per waiting
    /// thread per schedule point).
    #[inline]
    pub(crate) fn le(&self, other: &Clock) -> bool {
        self.0.iter().zip(other.0).fold(true, |all, (&mine, theirs)| all & (mine <= theirs))
    }

    /// The first thread whose part of `other` is past `self`'s: an event of it in `other` that
    /// `self` has not seen.
    #[inline]
    pub(crate) fn behind(&self, other: &Clock) -> Option<Tid> {
        self.0.iter().zip(other.0).position(|(&mine, theirs)| mine < theirs)
    }
}

/// One thread's event: its thread and its own clock then. `(0, 0)` happens before every event,
/// as an initial value does.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub(crate) struct Stamp {
    pub(crate) thread: u8,
    pub(crate) at: u32,
}

impl Stamp {
    pub(crate) const INIT: Stamp = Stamp { thread: 0, at: 0 };

    #[inline]
    pub(crate) fn before(self, clock: &Clock) -> bool {
        self.at <= clock.get(self.thread as Tid)
    }
}
