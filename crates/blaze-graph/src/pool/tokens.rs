//! A pool's core tokens on the machine's ledger ([`Tokens`]), and the permits its workers run
//! by: a process never runs more work threads than it holds tokens for (measured 2026-11-12:
//! 44 blaze processes on an 18-core machine each ran 16 work threads).
//!
//! A [`Gate`] is one word: the tokens the pool holds and the workers running on them. A worker
//! runs jobs only while it holds a permit ([`Gate::enter`]: one of the held tokens no other
//! worker runs on), and gives it back as it goes to sleep ([`Gate::leave`]). A permit is no
//! worker's own: any worker that wakes takes a free one, so every sleeping worker is the same
//! to a wake. The ledger is called only on those transitions, never per job:
//!
//! - at the start, the pool's first token whatever the machine's sum (each pool of the process
//!   runs at least one worker, so a job waiting on another pool's job never waits for a token
//!   no one will give), then up to the rest of its workers that fit ([`Gate::start`]);
//! - when a worker finds every permit taken while work is queued, the pool holds fewer than it
//!   wants and the ledger's epoch moved since its last take (another process gave some back,
//!   or this pool did): a take of what it lacks ([`Gate::grow`]);
//! - when the last running worker goes to sleep with nothing queued: back down to one
//!   ([`Gate::shrink`]);
//! - as the pool ends: all of them ([`Gate::give_all`]).
//!
//! The pool's held count rises only after the ledger granted and falls before the ledger is
//! told, so what the pool runs on is never more than the ledger counts for it. One ledger
//! operation of a pool runs at a time ([`Gate::busy`]); a worker that finds one under way does
//! not wait for it: a grow under way is the take it would have made, and a shrink skipped is
//! made by the worker of that grow, which runs, finds nothing and leaves last.
//!
//! A take or give the ledger refuses panics naming the ledger's error: a pool that cannot keep
//! its threads to its tokens stops (rule no-fallback); on a worker it stops the pool
//! ([`super::Broken`]).

use crate::{
    ledger::{MachineError, Tokens},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
};

#[cfg(test)]
mod tests;

/// One worker running, in a [`Gate`]'s word (its low half).
const RUN: u64 = 1;
/// One token held, in its high half.
const HOLD: u64 = 1 << 32;
/// [`Gate::seen`] after the pool's own give-back: its next need takes, whatever the epoch.
const GAVE: u64 = u64::MAX;

fn held(word: u64) -> u64 {
    word >> 32
}

fn running(word: u64) -> u64 {
    word & (HOLD - 1)
}

/// A pool's tokens and the permits its workers run on.
pub(crate) struct Gate {
    // dyn: the process's one driver (`super::set_tokens`), or a test's
    tokens: &'static dyn Tokens,
    /// The pool's workers: the most it takes.
    want: u64,
    /// Tokens held (high half) and workers running on them (low half), in one word so a
    /// permit is taken against what is held now.
    word: AtomicU64,
    /// The ledger's epoch as of the pool's last take, or [`GAVE`].
    seen: AtomicU64,
    /// A ledger operation of this pool is under way.
    busy: AtomicBool,
    /// The plant: [`Gate::enter`] ignores the tokens, as a pool before this module did.
    #[cfg(test)]
    pub(crate) plant: AtomicBool,
}

impl Gate {
    /// The gate of a pool of `want` workers: its first token whatever the machine's sum
    /// (`floor` past any count, on a take of one), then up to `want - 1` more that fit with a
    /// floor of 1 (already held, so only what fits).
    ///
    /// # Panics
    ///
    /// Where the ledger refuses a take, naming its error; and where it grants no first token,
    /// a ledger that broke [`Tokens::take`]'s floor.
    // dyn: the process's one driver, or a test's
    pub(crate) fn start(tokens: &'static dyn Tokens, want: u64) -> Gate {
        let want = want.max(1);
        let first = tokens.take(1, u64::MAX).unwrap_or_else(|why| failed("first take", 1, &why));
        assert_eq!(
            first.granted, 1,
            "a worker pool's first core token, taken with a floor past any count, was not granted: the ledger broke \
             `Tokens::take`'s floor, and a pool with no token runs nothing"
        );
        let (mut has, mut epoch) = (1, first.epoch);
        if want > 1 {
            let more =
                tokens.take(want - 1, 1).unwrap_or_else(|why| failed("take", want - 1, &why));
            has += more.granted;
            epoch = more.epoch;
        }
        Gate {
            tokens,
            want,
            word: AtomicU64::new(has * HOLD),
            seen: AtomicU64::new(epoch),
            busy: AtomicBool::new(false),
            #[cfg(test)]
            plant: AtomicBool::new(false),
        }
    }

    /// Takes a permit: true when one of the held tokens had no worker running on it. Lock-free:
    /// a failed exchange means another worker's succeeded.
    pub(crate) fn enter(&self) -> bool {
        #[cfg(test)]
        if self.plant.load(Ordering::Relaxed) {
            self.word.fetch_add(RUN, Ordering::SeqCst);
            return true;
        }
        let mut word = self.word.load(Ordering::SeqCst);
        while running(word) < held(word) {
            match self.word.compare_exchange_weak(
                word,
                word + RUN,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => word = now,
            }
        }
        false
    }

    /// Gives a permit back: whether it was the last one taken (no worker runs now).
    pub(crate) fn leave(&self) -> bool {
        running(self.word.fetch_sub(RUN, Ordering::SeqCst)) == 1
    }

    /// Whether a permit is free now.
    pub(crate) fn free(&self) -> bool {
        let word = self.word.load(Ordering::SeqCst);
        running(word) < held(word)
    }

    /// The tokens held now: the workers the pool may run at once.
    pub(crate) fn held(&self) -> usize {
        held(self.word.load(Ordering::SeqCst)) as usize
    }

    /// The workers running on a permit now.
    #[cfg(test)]
    pub(crate) fn running(&self) -> u64 {
        running(self.word.load(Ordering::SeqCst))
    }

    /// Takes what the pool lacks when it holds fewer than its workers, the ledger's epoch moved
    /// since its last take and work is `queued` (asked last, being the dearest to answer): how
    /// many it was granted (0 when it did not take, or nothing fit).
    ///
    /// # Panics
    ///
    /// Where the ledger refuses the take, naming its error.
    pub(crate) fn grow(&self, queued: impl FnOnce() -> bool) -> u64 {
        if held(self.word.load(Ordering::SeqCst)) >= self.want {
            return 0;
        }
        let seen = self.seen.load(Ordering::Acquire);
        if seen != GAVE && self.tokens.epoch() == seen {
            return 0;
        }
        if !queued() || self.busy.swap(true, Ordering::Acquire) {
            return 0;
        }
        let short = self.want.saturating_sub(held(self.word.load(Ordering::SeqCst)));
        let mut granted = 0;
        if short > 0 {
            let cores =
                self.tokens.take(short, 1).unwrap_or_else(|why| failed("take", short, &why));
            granted = cores.granted.min(short);
            self.word.fetch_add(granted * HOLD, Ordering::SeqCst);
            self.seen.store(cores.epoch, Ordering::Release);
        }
        self.busy.store(false, Ordering::Release);
        granted
    }

    /// Gives back what no worker runs on, keeping one (or as many as run now): what the last
    /// worker to sleep with nothing queued calls. How many it gave.
    ///
    /// # Panics
    ///
    /// Where the ledger refuses the give, naming its error.
    pub(crate) fn shrink(&self) -> u64 {
        if self.busy.swap(true, Ordering::Acquire) {
            return 0;
        }
        let gives = self.lower(|word| running(word).max(1));
        if gives > 0 {
            self.tokens.give(gives).unwrap_or_else(|why| failed("give", gives, &why));
            self.seen.store(GAVE, Ordering::Release);
        }
        self.busy.store(false, Ordering::Release);
        gives
    }

    /// Gives every token back, as the pool ends: once its workers have ended (a worker still
    /// running, a pool stopped by its own job, finds no permit after its job and ends).
    ///
    /// # Panics
    ///
    /// Where the ledger refuses the give, naming its error.
    pub(crate) fn give_all(&self) {
        crate::spin::SpinWait::until(
            "a pool's ledger operation under way, before it gives its tokens back",
            || !self.busy.swap(true, Ordering::Acquire),
        );
        let gives = self.lower(|_| 0);
        if gives > 0 {
            self.tokens.give(gives).unwrap_or_else(|why| failed("give", gives, &why));
        }
        self.seen.store(GAVE, Ordering::Release);
        self.busy.store(false, Ordering::Release);
    }

    /// Lowers the held count to `keep(word)` where it is above it: how many it lowered by.
    /// Lock-free, as [`Gate::enter`] is.
    fn lower(&self, keep: impl Fn(u64) -> u64) -> u64 {
        let mut word = self.word.load(Ordering::SeqCst);
        loop {
            let keep = keep(word);
            if held(word) <= keep {
                return 0;
            }
            let gives = held(word) - keep;
            match self.word.compare_exchange_weak(
                word,
                word - gives * HOLD,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return gives,
                Err(now) => word = now,
            }
        }
    }
}

#[cold]
fn failed(op: &str, cores: u64, why: &MachineError) -> ! {
    panic!(
        "a worker pool's {op} of {cores} core tokens on the machine's ledger failed: {why}; a pool that cannot keep \
         its threads to the tokens it holds stops (rule no-fallback)"
    )
}
