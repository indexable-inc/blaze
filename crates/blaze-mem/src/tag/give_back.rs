//! The handshake of a heap give-back (docs/memory-footprint.md, step 2): who asked one, which
//! threads have given back since, and so whether a purge may read the footprint yet.
//!
//! The global allocator (mimalloc, blaze-engine's `heaps.rs`) keeps freed pages per thread: a
//! page a thread's heap allocates from, or retired empty, and the blocks other threads freed
//! into it, are that thread's alone to collect. So a purge asks every thread to give back
//! ([`GiveBacks::ask`]) and each does its part as it next allocates for a task or goes idle.
//! Until each has, the footprint still holds what it kept: on 2026-11-09 the daemon re-read the
//! footprint at once after asking, and stopped at its 47 GiB ceiling with up to 7.5 GiB of the
//! global heap freed and resident.
//!
//! The threads that count are the pool's workers, each [`GiveBacks::join`]ed while it runs. A
//! counted thread that gave back for the generation asked says so ([`GiveBacks::gave`]); a purge
//! reads the footprint once every counted thread has ([`GiveBacks::settled`]), or when its own
//! deadline passes.
//!
//! One word holds it all: the generation asked (high 32 bits), the threads counted (16 bits)
//! and those that gave back for that generation (low 16 bits). Each operation is one atomic
//! read-modify-write of it, so an ask resets the acks in the same step it moves the generation,
//! an ack lands only on the generation its thread collected for, and a thread leaving takes its
//! own ack back with its count. The DST below interleaves them in every order Hegel draws.

use std::sync::atomic::{AtomicU64, Ordering};

const GENERATION_SHIFT: u32 = 32;
const COUNTED_SHIFT: u32 = 16;
const FIELD: u64 = (1 << 16) - 1;

/// The handshake's word ([the module's doc](self)).
#[derive(Debug)]
pub(crate) struct GiveBacks {
    word: AtomicU64,
    #[cfg(test)]
    plant: Plant,
}

/// A wrong handshake the DST's oracle must catch.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Plant {
    None,
    /// An ack lands on whatever generation is asked now, not only the one its thread collected
    /// for: a give-back that began before the ask counts for it.
    AckAnyGeneration,
    /// A thread that leaves takes its count away but leaves its ack: the acks then reach the
    /// count with a counted thread that never gave back.
    LeaveKeepsAck,
}

/// Where a purge's give-back stands ([`GiveBacks::settled`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Settle {
    /// Every counted thread gave back for `generation`, the one asked now: the generation the
    /// purge asked, or a later one, whose give-backs all began after it too.
    Settled { generation: u64 },
    /// `gave` of the `of` counted threads gave back for the generation asked now.
    Pending { gave: u64, of: u64 },
}

fn generation(word: u64) -> u64 {
    word >> GENERATION_SHIFT
}

fn counted(word: u64) -> u64 {
    (word >> COUNTED_SHIFT) & FIELD
}

fn acks(word: u64) -> u64 {
    word & FIELD
}

impl GiveBacks {
    pub(crate) const fn new() -> GiveBacks {
        GiveBacks {
            word: AtomicU64::new(0),
            #[cfg(test)]
            plant: Plant::None,
        }
    }

    #[cfg(test)]
    fn planted(plant: Plant) -> GiveBacks {
        GiveBacks { word: AtomicU64::new(0), plant }
    }

    /// One read-modify-write of the word: `f` gives the new word, or `None` to leave it.
    fn update(&self, f: impl FnMut(u64) -> Option<u64>) -> Result<u64, u64> {
        self.word.fetch_update(Ordering::AcqRel, Ordering::Acquire, f)
    }

    /// Asks every thread to give back: the generation moves on and its acks start at none, in
    /// one step. The generation asked.
    pub(crate) fn ask(&self) -> u64 {
        let before = self
            .update(|word| {
                let next = generation(word).wrapping_add(1) & u64::from(u32::MAX);
                Some(next << GENERATION_SHIFT | counted(word) << COUNTED_SHIFT)
            })
            .unwrap_or_else(|it| it);
        (generation(before) + 1) & u64::from(u32::MAX)
    }

    /// The generation asked now: what a thread holds what it last gave back for against.
    pub(crate) fn asked(&self) -> u64 {
        generation(self.word.load(Ordering::Acquire))
    }

    /// A counted thread that collected for `generation`, having read it from [`GiveBacks::asked`]
    /// before it began, says so: counted only while `generation` is still the one asked. Whether
    /// it counted.
    pub(crate) fn gave(&self, generation_given: u64) -> bool {
        #[cfg(test)]
        let any = self.plant == Plant::AckAnyGeneration;
        #[cfg(not(test))]
        let any = false;
        self.update(|word| {
            let current = any || generation(word) == generation_given;
            (current && acks(word) < counted(word)).then_some(word + 1)
        })
        .is_ok()
    }

    /// A thread begins to count: a pool worker as it starts.
    pub(crate) fn join(&self) {
        let _ = self.update(|word| (counted(word) < FIELD).then_some(word + (1 << COUNTED_SHIFT)));
    }

    /// A counted thread leaves, `acked` the generation its last counted give-back was for
    /// (`None`: none): its count goes, and its ack with it where that ack is for the
    /// generation asked now, so the acks never count a thread no longer counted.
    pub(crate) fn leave(&self, acked: Option<u64>) {
        #[cfg(test)]
        let keep = self.plant == Plant::LeaveKeepsAck;
        #[cfg(not(test))]
        let keep = false;
        let _ = self.update(|word| {
            if counted(word) == 0 {
                return None;
            }
            let mine = !keep && acked == Some(generation(word)) && acks(word) > 0;
            Some(word - (1 << COUNTED_SHIFT) - u64::from(mine))
        });
    }

    /// Whether every counted thread gave back for the generation asked now ([`Settle`]). Read
    /// by a purge after its own [`GiveBacks::ask`], the generation asked now is its own or a
    /// later one, whose give-backs began after its ask too, so they are what count.
    pub(crate) fn settled(&self) -> Settle {
        let word = self.word.load(Ordering::Acquire);
        let (now, of, gave) = (generation(word), counted(word), acks(word));
        match gave >= of {
            true => Settle::Settled { generation: now },
            false => Settle::Pending { gave, of },
        }
    }
}

#[cfg(test)]
mod tests;
