//! What blaze has seen of whether a proc-macro crate's expansions leave state, as
//! `blaze-macro-eval` keeps it per crate: the store's verdict when the crate loaded, and what
//! its runs in this process did since.

use std::sync::atomic::{AtomicBool, Ordering};

/// What the store says of a crate's expansions.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    /// None has run in a build that told the store.
    Unseen,
    /// Some ran, in a build, and none left state.
    Stateless,
    /// One left state.
    Stateful,
}

/// The store's value of [`Verdict::Stateless`].
pub const STATELESS: &[u8] = b"stateless";
/// The store's value of [`Verdict::Stateful`].
pub const STATEFUL: &[u8] = b"stateful";

/// A crate's verdict at load and what its runs did since, shared by the threads that run it.
#[derive(Debug)]
pub struct Seen {
    verdict: Verdict,
    /// Whether a run that left no state ran since the last [`Seen::flush`].
    ran: AtomicBool,
    /// Whether a run left state.
    retained: AtomicBool,
}

impl Seen {
    pub fn new(verdict: Verdict) -> Seen {
        Seen { verdict, ran: AtomicBool::new(false), retained: AtomicBool::new(false) }
    }

    /// The store's verdict when the crate loaded.
    pub fn verdict(&self) -> Verdict {
        self.verdict
    }

    /// Notes a run that left state (`retained`) or not. `Some` for the first to leave state
    /// when the store did not say so already: the caller tells the store [`STATEFUL`] at once
    /// and, with `Some(true)` (the store said stateless), says the store was wrong.
    pub fn saw(&self, retained: bool) -> Option<bool> {
        if !retained {
            self.ran.store(true, Ordering::Relaxed);
            return None;
        }
        if self.retained.swap(true, Ordering::Relaxed) || self.verdict == Verdict::Stateful {
            return None;
        }
        Some(self.verdict == Verdict::Stateless)
    }

    /// Whether to tell the store [`STATELESS`] once a build has expanded everything: the store
    /// said nothing at load, a run ran since the last flush, and none left state. The caller
    /// asks the store again first, since another build may have told it since the load.
    pub fn flush(&self) -> bool {
        self.verdict == Verdict::Unseen
            && self.ran.swap(false, Ordering::Relaxed)
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
                        assert_eq!(seen.saw(retained), old.saw(verdict, retained));
                        if flushes >> i & 1 == 1 {
                            assert_eq!(seen.flush(), old.flush(verdict));
                        }
                    }
                }
            }
        }
    }
}
