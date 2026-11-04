//! What blaze has seen of whether a proc-macro crate's expansions leave state, as
//! `blaze-macro-eval` keeps it per crate: the store's verdict when the crate loaded, and what
//! its runs in this process did since.
//!
//! A run's start is its calling crate's session's state: fresh (no call before it changed a
//! static), or what the calls before it left. What it did is [`Left`]: nothing, a change from
//! the fresh state ([`Left::First`]: the one-time set-up of a library such as proc-macro2's
//! `detection`, which every call made fresh would do again), or a change from a state calls
//! before it left ([`Left::Moved`]: a counter, a cache, state that keeps moving).

use std::sync::atomic::{AtomicBool, Ordering};

/// What the store says of a crate's expansions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// None has run in a build that told the store.
    Unseen,
    /// Some ran, in a build, and none left state.
    Stateless,
    /// Some left state, each from the fresh state ([`Left::First`]), and none changed what a
    /// call before it left: once a session's first call ran, its state stays.
    Settles,
    /// One changed what a call before it left ([`Left::Moved`]).
    Stateful,
}

/// The store's value of [`Verdict::Stateless`].
pub const STATELESS: &[u8] = b"stateless";
/// The store's value of [`Verdict::Settles`].
pub const SETTLES: &[u8] = b"settles";
/// The store's value of [`Verdict::Stateful`].
pub const STATEFUL: &[u8] = b"stateful";

/// What one run did to its session's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Left {
    /// It left the state it started from as it was.
    Nothing,
    /// It started from the fresh state and changed it, to a state a next run can be given.
    First,
    /// It changed a state calls before it left, or ran after them again (it left an address,
    /// which no run can be given, so each next run replays them): its state moves on.
    Moved,
}

/// What a run tells the store at once: the verdict's value, and whether it contradicts the
/// store's verdict at load ([`Verdict::Stateless`]), under which this build's calls ran, or
/// were found, as if each started fresh.
pub type Tell = (&'static [u8], bool);

/// A crate's verdict at load and what its runs did since, shared by the threads that run it.
#[derive(Debug)]
pub struct Seen {
    verdict: Verdict,
    /// Whether a run that left no state ran since the last [`Seen::flush`].
    ran: AtomicBool,
    /// Whether a run left state from the fresh state ([`Left::First`]).
    first: AtomicBool,
    /// Whether a run moved state on ([`Left::Moved`]).
    retained: AtomicBool,
}

impl Seen {
    pub fn new(verdict: Verdict) -> Seen {
        Seen {
            verdict,
            ran: AtomicBool::new(false),
            first: AtomicBool::new(false),
            retained: AtomicBool::new(false),
        }
    }

    /// The store's verdict when the crate loaded.
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Notes what a run `left`. `Some` for the first run to show more state than the store
    /// said: the caller tells the store the value at once and, when it contradicts, says the
    /// store was wrong. Telling at once, before the build's [`Seen::flush`], keeps another
    /// build's [`STATELESS`] from winning: the store's [`STATEFUL`] beats its [`SETTLES`],
    /// which beats its [`STATELESS`].
    pub fn saw(&self, left: Left) -> Option<Tell> {
        let contradicted = self.verdict == Verdict::Stateless;
        match left {
            Left::Nothing => {
                self.ran.store(true, Ordering::Relaxed);
                None
            }
            Left::First => {
                if matches!(self.verdict, Verdict::Settles | Verdict::Stateful)
                    || self.retained.load(Ordering::Relaxed)
                    || self.first.swap(true, Ordering::Relaxed)
                {
                    return None;
                }
                Some((SETTLES, contradicted))
            }
            Left::Moved => {
                if self.retained.swap(true, Ordering::Relaxed) || self.verdict == Verdict::Stateful
                {
                    return None;
                }
                Some((STATEFUL, contradicted))
            }
        }
    }

    /// Whether to tell the store [`STATELESS`] once a build has expanded everything: the store
    /// said nothing at load, a run ran since the last flush, and none left state. The caller
    /// asks the store again first, since another build may have told it since the load.
    pub fn flush(&self) -> bool {
        self.verdict == Verdict::Unseen
            && self.ran.swap(false, Ordering::Relaxed)
            && !self.first.load(Ordering::Relaxed)
            && !self.retained.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tracking `blaze-macro-eval` had before this module (`Library::saw` and its `flush`),
    /// told as what it did.
    #[derive(Default)]
    struct Old {
        ran: bool,
        retained: bool,
    }

    impl Old {
        fn saw(&mut self, verdict: Verdict, retained: bool) -> Option<bool> {
            if !retained {
                self.ran = true;
                return None;
            }
            if std::mem::replace(&mut self.retained, true) || verdict == Verdict::Stateful {
                return None;
            }
            Some(verdict == Verdict::Stateless)
        }

        fn flush(&mut self, verdict: Verdict) -> bool {
            verdict == Verdict::Unseen && std::mem::replace(&mut self.ran, false) && !self.retained
        }
    }

    #[test]
    fn tells_what_the_hosts_told() {
        for verdict in [Verdict::Unseen, Verdict::Stateless, Verdict::Stateful] {
            // Every sequence of up to six runs, each leaving state or not, with a flush after
            // each run whose bit says so.
            for runs in 0u32..1 << 6 {
                for flushes in 0u32..1 << 6 {
                    let (seen, mut old) = (Seen::new(verdict), Old::default());
                    for i in 0..6 {
                        let retained = runs >> i & 1 == 1;
                        let left = if retained { Left::Moved } else { Left::Nothing };
                        let told =
                            old.saw(verdict, retained).map(|contradicted| (STATEFUL, contradicted));
                        assert_eq!(seen.saw(left), told);
                        if flushes >> i & 1 == 1 {
                            assert_eq!(seen.flush(), old.flush(verdict));
                        }
                    }
                }
            }
        }
    }

    /// A crate whose runs change only the fresh state settles: it is told once, at once, and
    /// a flush tells nothing more; a run that moves state on after that makes it stateful.
    #[test]
    fn a_change_from_the_fresh_state_settles() {
        let seen = Seen::new(Verdict::Unseen);
        assert_eq!(seen.saw(Left::First), Some((SETTLES, false)));
        assert_eq!(seen.saw(Left::Nothing), None);
        assert_eq!(seen.saw(Left::First), None);
        assert!(!seen.flush());
        assert_eq!(seen.saw(Left::Moved), Some((STATEFUL, false)));
        assert_eq!(seen.saw(Left::First), None);

        // The store said stateless: this build ran its calls each from the fresh state.
        let seen = Seen::new(Verdict::Stateless);
        assert_eq!(seen.saw(Left::First), Some((SETTLES, true)));
        // Settled already, or stateful: nothing new to tell.
        assert_eq!(Seen::new(Verdict::Settles).saw(Left::First), None);
        assert_eq!(Seen::new(Verdict::Settles).saw(Left::Moved), Some((STATEFUL, false)));
        assert_eq!(Seen::new(Verdict::Stateful).saw(Left::First), None);
    }
}
