//! [`Slots`]: the program's table of slot words, as its control thread keeps it.
//!
//! One word a function. A call loads its word and jumps where it names: the slot's stub, a fast
//! body or an optimized body. A stub's call raises a need (the control thread asks the parent,
//! or places a record it already knows); the answer is a [`Slots::publish`], whose writes place
//! the code, then store the word (one aligned atomic store), then, when a body was replaced,
//! publish the epoch. A word never names code that is not whole: [`Write::Place`] comes before
//! the [`Write::Store`] that names it, and a word is one `u64` ([`Word`]), so no thread can
//! see half of it.
//!
//! A replaced body is retired, never freed while a frame can be in it. This is blaze-launch's
//! epoch protocol (`blaze_launch::launch`, promise 4) with one body a batch, since a first call
//! and a promotion each mean what the code they replace meant and need no gate:
//!
//! - The publish that unlinks a body stamps it with the epoch it then publishes, after the store.
//! - A thread reports an epoch it loaded *before* it walked its stack, and the frames of its
//!   stack that are in placed code. It clears each retired body stamped at or before that epoch
//!   that none of its frames is in. Once clear, always: no word names the body any more.
//! - A thread is told to the core with the epoch its parent loaded before starting it, and holds
//!   only what is retired after that; a thread that ends holds nothing.
//! - [`Slots::collect`] frees a body once every thread that was alive at its stamp cleared it.
//!
//! What the driver keeps (the launcher's control thread, blaze-launch-rt's `drive`): it carries
//! each call's writes in order, all of them before the next call that changes the table; a
//! store and the epoch are each one atomic store with release order, a thread's loads acquire.

#![blaze::sans_io]

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::{CodeRef, Plant, SlotId, ThreadId, Tier};

/// A placed body, by its number among every body the program ever placed: the offset the code
/// area handed out for it, which is never handed out again (launcher.md, "Code is appended").
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Body(pub u32);

/// What one slot's word holds, all in one `u64`: 0 is the stub; else the body's number plus one,
/// with the top bit set for optimized code. In the program the word holds an address; the tier
/// is then the body's, found from the address. One store writes all of it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Word(pub u64);

const OPTIMIZED_BIT: u64 = 1 << 63;

impl Word {
    pub const STUB: Word = Word(0);

    pub const fn of(body: Body, tier: Tier) -> Word {
        let bit = match tier {
            Tier::Fast => 0,
            Tier::Optimized => OPTIMIZED_BIT,
        };
        Word((body.0 as u64 + 1) | bit)
    }

    /// What a call that loaded this word runs.
    pub const fn names(self) -> Names {
        if self.0 == 0 {
            return Names::Stub;
        }
        let tier = if self.0 & OPTIMIZED_BIT != 0 { Tier::Optimized } else { Tier::Fast };
        Names::Code { body: Body(((self.0 & !OPTIMIZED_BIT) - 1) as u32), tier }
    }
}

/// What a slot's word names: its state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Names {
    /// Not compiled here yet: a call raises a need.
    Stub,
    Code { body: Body, tier: Tier },
}

/// One thing for the driver to do, in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Write {
    /// Read `code` from the store, copy it to `body`'s range, apply its fixups and check it
    /// against its record's digest. No word names it yet.
    Place { body: Body, slot: SlotId, code: CodeRef },
    /// Store `word` in `slot`'s word.
    Store { slot: SlotId, word: Word },
    /// Publish the epoch where threads load it.
    Epoch { epoch: u64 },
    /// No thread is or can be in `body`: its range, unwind entry and symbol go.
    Free { body: Body },
}

/// Why the core refused a call. Each names what it is about; none changes the core's state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    /// A table of no slot, or of more than a word can number.
    Config { what: &'static str, value: u64 },
    NoSlot { slot: SlotId, slots: u32 },
    /// Fast code for a slot that has fast code: two compiles of one function (single flight
    /// broken upstream).
    FastTwice { slot: SlotId },
    /// Anything for a slot that has optimized code: nothing is above it in one run.
    AlreadyOptimized { slot: SlotId, tier: Tier },
    /// More bodies than the code area numbers: the program is restarted in place.
    Full { most: u32 },
    NoThread { thread: ThreadId },
    ThreadTwice { thread: ThreadId },
    /// An epoch a thread cannot have loaded: past the last published.
    Epoch { thread: ThreadId, said: u64, published: u64 },
    /// A frame in a body that was never placed.
    NoBody { thread: ThreadId, body: Body },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the slot table: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "slot {} is past the table's {slots} slots", slot.0)
            }
            Error::FastTwice { slot } => write!(
                f,
                "slot {} was given fast code twice: its function was compiled twice",
                slot.0
            ),
            Error::AlreadyOptimized { slot, tier } => write!(
                f,
                "slot {} has optimized code and was given {tier:?} code: nothing is above it",
                slot.0
            ),
            Error::Full { most } => write!(
                f,
                "the code area has numbered all its {most} bodies: the program must start again"
            ),
            Error::NoThread { thread } => write!(f, "thread {} was never told", thread.0),
            Error::ThreadTwice { thread } => write!(f, "thread {} was told twice", thread.0),
            Error::Epoch { thread, said, published } => write!(
                f,
                "thread {} reported epoch {said}, past the last published, {published}",
                thread.0
            ),
            Error::NoBody { thread, body } => write!(
                f,
                "thread {} reported a frame in body {}, which was never placed",
                thread.0, body.0
            ),
        }
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    /// Its slot's word names it.
    Bound,
    /// Replaced; waits for every thread to clear it.
    Retired,
    Freed,
}

#[derive(Clone, Copy, Debug)]
struct Placed {
    slot: SlotId,
    tier: Tier,
    code: CodeRef,
    life: Life,
}

/// One retired body and the threads that have not cleared it.
struct Batch<A: Allocator> {
    /// The epoch the publish that unlinked it published, after its store.
    stamp: u64,
    body: Body,
    waiting: Vec<ThreadId, A>,
}

/// A body as the core knows it: what a program counter in the area names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    pub slot: SlotId,
    pub tier: Tier,
    pub code: CodeRef,
    /// Whether its slot's word names it now; else it is retired or freed.
    pub bound: bool,
    pub freed: bool,
}

/// The table of one program. See the module's documentation.
pub struct Slots<A: Allocator + Clone> {
    alloc: A,
    words: Vec<Word, A>,
    bodies: Vec<Placed, A>,
    batches: Vec<Batch<A>, A>,
    /// Every thread alive, as told.
    threads: Vec<ThreadId, A>,
    epoch: u64,
    /// The most bodies the code area numbers.
    most_bodies: u32,
    pub(crate) plant: Plant,
}

impl<A: Allocator + Clone> Slots<A> {
    /// A table of `slots` stubs, whose area numbers at most `most_bodies` bodies, in `alloc`.
    pub fn new(slots: u32, most_bodies: u32, alloc: A) -> Result<Slots<A>, Error> {
        if slots == 0 {
            return Err(Error::Config { what: "a table of no slot", value: 0 });
        }
        if u64::from(most_bodies) >= OPTIMIZED_BIT {
            return Err(Error::Config {
                what: "more bodies than a word numbers",
                value: u64::from(most_bodies),
            });
        }
        let mut words = Vec::new_in(alloc.clone());
        words.resize(slots as usize, Word::STUB);
        Ok(Slots {
            words,
            bodies: Vec::new_in(alloc.clone()),
            batches: Vec::new_in(alloc.clone()),
            threads: Vec::new_in(alloc.clone()),
            alloc,
            epoch: 0,
            most_bodies,
            plant: Plant::None,
        })
    }

    pub fn slots(&self) -> u32 {
        self.words.len() as u32
    }

    /// The epoch last published.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// What `slot`'s word names, as the core last decided it (the driver's memory holds it once
    /// the writes are carried out).
    pub fn word(&self, slot: SlotId) -> Result<Word, Error> {
        self.words.get(slot.index()).copied().ok_or(Error::NoSlot { slot, slots: self.slots() })
    }

    /// Retired bodies not freed yet.
    pub fn retired(&self) -> usize {
        self.batches.len()
    }

    pub fn found(&self, body: Body) -> Option<Found> {
        let placed = self.bodies.get(body.0 as usize)?;
        Some(Found {
            slot: placed.slot,
            tier: placed.tier,
            code: placed.code,
            bound: placed.life == Life::Bound,
            freed: placed.life == Life::Freed,
        })
    }

    /// Makes `code` what `slot` runs, at `tier`: a new body placed, then the word stored; the
    /// body it replaces, if any, retired under the epoch published last. A stub takes either
    /// tier, fast code only optimized code, optimized code nothing.
    pub fn publish(
        &mut self,
        slot: SlotId,
        tier: Tier,
        code: CodeRef,
        out: &mut Vec<Write, A>,
    ) -> Result<Body, Error> {
        let old = self.word(slot)?.names();
        match (old, tier) {
            (Names::Stub, _) | (Names::Code { tier: Tier::Fast, .. }, Tier::Optimized) => {}
            (Names::Code { tier: Tier::Fast, .. }, Tier::Fast) => {
                return Err(Error::FastTwice { slot });
            }
            (Names::Code { tier: Tier::Optimized, .. }, tier) => {
                return Err(Error::AlreadyOptimized { slot, tier });
            }
        }
        if self.bodies.len() as u64 >= u64::from(self.most_bodies) {
            return Err(Error::Full { most: self.most_bodies });
        }
        let body = Body(self.bodies.len() as u32);
        self.bodies.push(Placed { slot, tier, code, life: Life::Bound });
        let word = Word::of(body, tier);
        self.words[slot.index()] = word;
        match self.plant {
            Plant::StoreBeforePlace => {
                out.push(Write::Store { slot, word });
                out.push(Write::Place { body, slot, code });
            }
            _ => {
                out.push(Write::Place { body, slot, code });
                out.push(Write::Store { slot, word });
            }
        }
        if let Names::Code { body: old, .. } = old {
            self.bodies[old.0 as usize].life = Life::Retired;
            let epoch = self.epoch + 1;
            self.epoch = epoch;
            let mut waiting = Vec::with_capacity_in(self.threads.len(), self.alloc.clone());
            waiting.extend(self.threads.iter().copied());
            self.batches.push(Batch { stamp: epoch, body: old, waiting });
            out.push(Write::Epoch { epoch });
        }
        Ok(body)
    }

    /// A thread began, having loaded `epoch` before it could run any of the program's code (its
    /// parent loaded it before starting it): it holds what is retired after that until it reports.
    pub fn thread_began(&mut self, thread: ThreadId, epoch: u64) -> Result<(), Error> {
        if self.threads.contains(&thread) {
            return Err(Error::ThreadTwice { thread });
        }
        if epoch > self.epoch {
            return Err(Error::Epoch { thread, said: epoch, published: self.epoch });
        }
        self.threads.push(thread);
        for batch in self.batches.iter_mut().filter(|batch| batch.stamp > epoch) {
            batch.waiting.push(thread);
        }
        Ok(())
    }

    /// A thread ended: it is in no code any more.
    pub fn thread_ended(&mut self, thread: ThreadId) -> Result<(), Error> {
        let Some(at) = self.threads.iter().position(|known| *known == thread) else {
            return Err(Error::NoThread { thread });
        };
        self.threads.swap_remove(at);
        for batch in &mut self.batches {
            batch.waiting.retain(|waits| *waits != thread);
        }
        Ok(())
    }

    /// A thread's stack after it loaded `epoch`: the bodies its frames are in, and whether the
    /// walk reached the stack's end (one that gave up holds everything). Reports are taken in any
    /// order: what a thread cleared stays cleared.
    pub fn report(
        &mut self,
        thread: ThreadId,
        epoch: u64,
        frames: &[Body],
        complete: bool,
    ) -> Result<(), Error> {
        if epoch > self.epoch {
            return Err(Error::Epoch { thread, said: epoch, published: self.epoch });
        }
        if let Some(body) = frames.iter().find(|body| body.0 as usize >= self.bodies.len()) {
            return Err(Error::NoBody { thread, body: *body });
        }
        if !self.threads.contains(&thread) {
            return Err(Error::NoThread { thread });
        }
        for batch in self.batches.iter_mut().filter(|batch| batch.stamp <= epoch) {
            let Some(at) = batch.waiting.iter().position(|waits| *waits == thread) else {
                continue;
            };
            if complete && !frames.contains(&batch.body) {
                batch.waiting.swap_remove(at);
            }
        }
        Ok(())
    }

    /// Frees every retired body every thread cleared, the oldest first: how many.
    pub fn collect(&mut self, out: &mut Vec<Write, A>) -> usize {
        let mut freed = 0;
        let mut at = 0;
        while at < self.batches.len() {
            if !self.batches[at].waiting.is_empty() && self.plant != Plant::FreeEarly {
                at += 1;
                continue;
            }
            let batch = self.batches.remove(at);
            self.bodies[batch.body.0 as usize].life = Life::Freed;
            out.push(Write::Free { body: batch.body });
            freed += 1;
        }
        freed
    }
}
