//! [`Slots`]: the program's two words a function, as its control thread keeps them.
//!
//! - **The slot word** is what every caller loads and jumps through: the need stub (no code yet),
//!   the function's body itself (nothing observed: a call pays nothing), or a watch stub.
//! - **The body word** is the function's current code: the need stub or a body. A watch stub
//!   does what it watches ([`Watch`]: a count toward heat's bound, a test's mark, the viewer's
//!   hooks), then jumps through the body word.
//!
//! No instrumentation is ever compiled into a body, so a body's bytes are the same whatever is
//! watched, and a code record's key holds nothing about it ([`crate::CodeKey`]).
//!
//! The rule that keeps the two words true together: [`Slots::publish`] places the new body,
//! stores the body word, and stores the slot word too only when it held the old body word (no
//! stub in it); [`Slots::observe`] and [`Slots::plain`] change the slot's watch and store the
//! slot word as a stub, or as the body word *now* when the watch is empty. The control thread
//! is the one writer of both words: a stub's runtime call (a count reaching zero) only queues its
//! slot for the control thread and stores nothing, so a publish and an observe or plain are never
//! two writers racing on a word, whatever order their requests come in. Each call's writes are
//! carried in order after the last call's, so memory goes through the states the core went
//! through. A word never names code that is not whole: [`Write::Place`] comes before any store
//! that names it, and a word is one `u64` ([`Word`]), so no thread can see half of it.
//!
//! A replaced body is retired, never freed while a frame can be in it. This is blaze-launch's
//! epoch protocol (`blaze_launch::launch`, promise 4) with one body a batch, since a first call
//! and a promotion each mean what the code they replace meant and need no gate:
//!
//! - The publish that unlinks a body stamps it with the epoch it then publishes, after both
//!   stores: from then no word names it.
//! - A thread reports an epoch it loaded *before* it walked its stack, and the frames of its
//!   stack that are in placed code. It clears each retired body stamped at or before that epoch
//!   that none of its frames is in. Once clear, always: no word names the body any more. A stub
//!   is no safepoint, so a thread between a stub's load of the body word and its jump is not
//!   reported there, as a caller between its load of the slot word and its call is not.
//! - A thread is told to the core with the epoch its parent loaded before starting it, and holds
//!   only what is retired after that; a thread that ends holds nothing.
//! - [`Slots::collect`] frees a body once every thread that was alive at its stamp cleared it.
//!
//! What the driver keeps (the launcher's control thread, blaze-launch-rt's `drive`): it carries
//! each call's writes in order; a publish or a collect only once every earlier write is carried
//! (an observe or plain may come at any time: its writes follow the earlier ones in the queue);
//! a store and the epoch are each one atomic store with release order, a thread's loads acquire.
//! A `collect` before a publish's writes are carried frees a body while the old word is still in
//! memory, and a thread begun then can enter it: the core refuses such a thread
//! ([`Error::BehindFree`]), never runs it.

#![blaze::sans_io]

use core::fmt;

use blaze_own::{AVec, Alloc, Vec, soa};

use crate::{CodeRef, Plant, SlotId, ThreadId, Tier, Watch};

/// A placed body, by its number among every body the program ever placed: the offset the code
/// area handed out for it, which is never handed out again (launcher.md, "Code is appended").
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Body(pub u32);

/// What one word holds, all in one `u64`: 0 is the need stub; with the stub bit set, the slot's
/// watch stub for the watch in the low bits; else the body's number plus one, with the top bit
/// set for optimized code. In the program a word holds an address: of the need stub, of the
/// slot's stub for that watch, or of the body (whose tier is found from it). One store writes it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Word(pub u64);

const OPTIMIZED_BIT: u64 = 1 << 63;
const STUB_BIT: u64 = 1 << 62;

impl Word {
    pub const NEED: Word = Word(0);

    pub const fn of(body: Body, tier: Tier) -> Word {
        let bit = match tier {
            Tier::Fast => 0,
            Tier::Optimized => OPTIMIZED_BIT,
        };
        Word((body.0 as u64 + 1) | bit)
    }

    pub const fn stub(watch: Watch) -> Word {
        Word(STUB_BIT | watch.0 as u64)
    }

    /// What a call that loaded this word runs.
    pub const fn names(self) -> Names {
        if self.0 == 0 {
            return Names::Need;
        }
        if self.0 & STUB_BIT != 0 {
            return Names::Stub(Watch(self.0 as u16));
        }
        let tier = if self.0 & OPTIMIZED_BIT != 0 { Tier::Optimized } else { Tier::Fast };
        Names::Code { body: Body(((self.0 & !OPTIMIZED_BIT) - 1) as u32), tier }
    }
}

/// What a word names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Names {
    /// Not compiled here yet: a call raises a need.
    Need,
    /// A watch stub: it does what it watches, then jumps through the body word. Only ever in a
    /// slot word.
    Stub(Watch),
    Code { body: Body, tier: Tier },
}

/// One thing for the driver to do, in the order given.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Write {
    /// Read `code` from the store, copy it to `body`'s range, apply its fixups and check it
    /// against its record's digest. No word names it yet.
    Place { body: Body, slot: SlotId, code: CodeRef },
    /// Store `word` in `slot`'s body word: what its stubs jump to.
    Body { slot: SlotId, word: Word },
    /// Store `word` in `slot`'s slot word: what its callers jump to.
    Slot { slot: SlotId, word: Word },
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
    /// The code the body word names already, published again: two publishes of one record
    /// (single flight broken upstream).
    Same { slot: SlotId, code: CodeRef },
    /// More bodies than the code area numbers: the program is restarted in place.
    Full { most: u32 },
    NoThread { thread: ThreadId },
    ThreadTwice { thread: ThreadId },
    /// An epoch a thread cannot have loaded: past the last published.
    Epoch { thread: ThreadId, said: u64, published: u64 },
    /// A frame in a body that was never placed.
    NoBody { thread: ThreadId, body: Body },
    /// A thread began on an epoch older than a body already freed: it may have loaded that
    /// body's word, so it may be in freed code. The driver carried a [`Write::Free`] before the
    /// [`Write::Epoch`] it follows, or began the thread on an epoch loaded before that.
    BehindFree { thread: ThreadId, epoch: u64, freed: u64 },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Error::Config { what, value } => write!(f, "the slot table: {what} ({value})"),
            Error::NoSlot { slot, slots } => {
                write!(f, "slot {} is past the table's {slots} slots", slot.0)
            }
            Error::Same { slot, code } => write!(
                f,
                "slot {} was given the code it runs, {:02x?}, again: one record published twice",
                slot.0,
                &code.0[..8]
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
            Error::BehindFree { thread, epoch, freed } => write!(
                f,
                "thread {} began on epoch {epoch}, older than a body freed at epoch {freed}: it may run freed code",
                thread.0
            ),
        }
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Life {
    /// Its slot's body word names it.
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
struct Batch<'a> {
    /// The epoch the publish that unlinked it published, after its stores.
    stamp: u64,
    body: Body,
    waiting: AVec<'a, ThreadId>,
}

/// A body as the core knows it: what a program counter in the area names.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Found {
    pub slot: SlotId,
    pub tier: Tier,
    pub code: CodeRef,
    /// Whether its slot's body word names it now; else it is retired or freed.
    pub bound: bool,
    pub freed: bool,
}

soa! {
    /// One slot, by columns: a call loads `slot_words` alone, a stub `body_words` alone.
    struct Row, Rows, RowRef {
        slot_word: Word => slot_words / slot_words_mut,
        body_word: Word => body_words / body_words_mut,
        watch: Watch => watches / watches_mut,
    }
}

/// The table of one program. See the module's documentation.
pub struct Slots<'a> {
    alloc: Alloc<'a>,
    table: Rows<Alloc<'a>>,
    bodies: AVec<'a, Placed>,
    batches: AVec<'a, Batch<'a>>,
    /// Every thread alive, as told.
    threads: AVec<'a, ThreadId>,
    epoch: u64,
    /// The newest stamp of a body freed: no thread may begin on an older epoch.
    freed_through: u64,
    /// The most bodies the code area numbers.
    most_bodies: u32,
    pub(crate) plant: Plant,
}

impl<'a> Slots<'a> {
    /// A table of `slots` slots, each at its need stub and watching nothing, whose area numbers
    /// at most `most_bodies` bodies, in `alloc`.
    pub fn new(slots: u32, most_bodies: u32, alloc: Alloc<'a>) -> Result<Slots<'a>, Error> {
        if slots == 0 {
            return Err(Error::Config { what: "a table of no slot", value: 0 });
        }
        if u64::from(most_bodies) >= STUB_BIT {
            return Err(Error::Config {
                what: "more bodies than a word numbers",
                value: u64::from(most_bodies),
            });
        }
        let mut table = Rows::with_capacity_in(slots as usize, alloc);
        for _ in 0..slots {
            table.push(Row { slot_word: Word::NEED, body_word: Word::NEED, watch: Watch::NONE });
        }
        Ok(Slots {
            table,
            bodies: Vec::new_in(alloc),
            batches: Vec::new_in(alloc),
            threads: Vec::new_in(alloc),
            alloc,
            epoch: 0,
            freed_through: 0,
            most_bodies,
            plant: Plant::None,
        })
    }

    pub fn slots(&self) -> u32 {
        self.table.len() as u32
    }

    /// The epoch last published.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    fn check(&self, slot: SlotId) -> Result<(), Error> {
        match slot.index() < self.table.len() {
            true => Ok(()),
            false => Err(Error::NoSlot { slot, slots: self.slots() }),
        }
    }

    /// `slot`'s slot word, body word and watch, as the core last decided them (the driver's
    /// memory holds them once the writes are carried out).
    pub fn words(&self, slot: SlotId) -> Result<(Word, Word, Watch), Error> {
        self.check(slot)?;
        let at = slot.index();
        Ok((self.table.slot_words()[at], self.table.body_words()[at], self.table.watches()[at]))
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

    /// Makes `code` what `slot` runs, at `tier`: a new body placed, the body word stored, the
    /// slot word stored too when it held the old body word (never over a stub, whose watch goes
    /// on, now in front of the new body); the body it replaces, if any, retired under the epoch
    /// it publishes. A first call and a promotion each mean what the function means; the code
    /// the body word names already is refused.
    pub fn publish(
        &mut self,
        slot: SlotId,
        tier: Tier,
        code: CodeRef,
        out: &mut AVec<'_, Write>,
    ) -> Result<Body, Error> {
        self.check(slot)?;
        let at = slot.index();
        let old = self.table.body_words()[at];
        if let Names::Code { body, .. } = old.names()
            && self.bodies[body.0 as usize].code == code
        {
            return Err(Error::Same { slot, code });
        }
        if self.bodies.len() as u64 >= u64::from(self.most_bodies) {
            return Err(Error::Full { most: self.most_bodies });
        }
        let body = Body(self.bodies.len() as u32);
        self.bodies.push(Placed { slot, tier, code, life: Life::Bound });
        let word = Word::of(body, tier);
        self.table.body_words_mut()[at] = word;
        let plain = self.table.slot_words()[at] == old || self.plant == Plant::PublishOverStub;
        let early = self.plant == Plant::StoreBeforePlace;
        if !early {
            out.push(Write::Place { body, slot, code });
        }
        out.push(Write::Body { slot, word });
        if plain {
            self.table.slot_words_mut()[at] = word;
            out.push(Write::Slot { slot, word });
        }
        if early {
            out.push(Write::Place { body, slot, code });
        }
        if let Names::Code { body: old, .. } = old.names() {
            self.bodies[old.0 as usize].life = Life::Retired;
            let epoch = self.epoch + 1;
            self.epoch = epoch;
            let mut waiting = Vec::with_capacity_in(self.threads.len(), self.alloc);
            waiting.extend(self.threads.iter().copied());
            self.batches.push(Batch { stamp: epoch, body: old, waiting });
            out.push(Write::Epoch { epoch });
        }
        Ok(body)
    }

    /// Turns `watch` on for `slot`: its slot word becomes the stub for the slot's whole watch.
    pub fn observe(&mut self, slot: SlotId, watch: Watch, out: &mut AVec<'_, Write>) -> Result<(), Error> {
        self.watch(slot, watch, true, out)
    }

    /// Turns `watch` off for `slot`: its slot word becomes the stub for what is still watched,
    /// or, with nothing left, the body word as it is now (never one remembered from before).
    pub fn plain(&mut self, slot: SlotId, watch: Watch, out: &mut AVec<'_, Write>) -> Result<(), Error> {
        self.watch(slot, watch, false, out)
    }

    fn watch(&mut self, slot: SlotId, watch: Watch, on: bool, out: &mut AVec<'_, Write>) -> Result<(), Error> {
        self.check(slot)?;
        let at = slot.index();
        let now = self.table.watches()[at].with(watch, on);
        self.table.watches_mut()[at] = now;
        let word = match now.is_none() {
            true => self.table.body_words()[at],
            false => Word::stub(now),
        };
        if self.table.slot_words()[at] != word {
            self.table.slot_words_mut()[at] = word;
            out.push(Write::Slot { slot, word });
        }
        Ok(())
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
        if epoch < self.freed_through {
            return Err(Error::BehindFree { thread, epoch, freed: self.freed_through });
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
    pub fn collect(&mut self, out: &mut AVec<'_, Write>) -> usize {
        let mut freed = 0;
        let mut at = 0;
        while at < self.batches.len() {
            if !self.batches[at].waiting.is_empty() && self.plant != Plant::FreeEarly {
                at += 1;
                continue;
            }
            let batch = self.batches.remove(at);
            self.freed_through = self.freed_through.max(batch.stamp);
            self.bodies[batch.body.0 as usize].life = Life::Freed;
            out.push(Write::Free { body: batch.body });
            freed += 1;
        }
        freed
    }
}
