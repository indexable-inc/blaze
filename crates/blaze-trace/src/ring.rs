//! The rings: one single-producer ring of fixed-size entries a thread, in one fixed table
//! ([`Emitter`]), so a thread records without a lock, a wait or a read-modify-write (the
//! 128-core rule), and a reader drains every ring without ever holding a producer back. The one
//! home of the rings: blaze-watch's events (64-byte slots) and the span edges of this crate
//! (32-byte slots, [`crate::record`]) are two widths of it.
//!
//! **A slot** is `W` words: a stamp word and `W - 1` payload words. `W` divides the 8 words of a
//! cache line, so a slot never straddles two lines, and a ring takes whole lines
//! ([`Ring::words`]), so two producers never share a line; a producer shares one with the reader
//! only on the slots it writes.
//!
//! **The protocol** is a seqlock with every word atomic (so no data race exists to reason about,
//! and no `unsafe`): Boehm's form ("Can seqlocks get along with programming language memory
//! models?", 2012). Entry `n` (0-based) is in slot `n mod len`; its stamp is `(n + 1) << 1`, odd
//! while it is written. A push is the fixed list [`push_ops`]:
//!
//! 1. the stamp, odd (`Relaxed`): marks the slot busy;
//! 2. a `Release` fence: a reader that sees any payload word of this write, and then fences
//!    `Acquire`, sees step 1 or later in its second stamp load, so it cannot accept a torn copy;
//! 3. the payload words (`Relaxed`);
//! 4. the stamp, even (`Release`): a reader that loads it `Acquire` sees the whole payload;
//! 5. the ring's head, `n + 1` (`Release`): a reader that loads it `Acquire` knows every entry
//!    below is stamped (or overwritten by a later lap, which its stamp says).
//!
//! The producer is the ring's only writer, so the head is a store, not a `fetch_add`: no
//! read-modify-write and no contention. A push is the same operations whatever any reader does:
//! wait-free. When the ring is full the oldest entry is overwritten.
//!
//! **A drain** ([`Drain`], a step machine, one atomic operation a step) loads the head
//! (`Acquire`), and for each entry from its cursor: the stamp (`Acquire`), the payload
//! (`Relaxed`), an `Acquire` fence, the stamp again (`Relaxed`); the entry is kept only when
//! both stamps are its own even stamp. Entries the producer lapped before the drain reached
//! them, and entries overwritten during the copy, are reported as [`Got::Lost`] with their
//! numbers, so `delivered + lost` is exactly the number of entries pushed since the cursor, and
//! an entry is lost only when `len` newer ones were pushed: only the oldest are lost.
//!
//! **A thread's lane** is claimed once, when the thread first records (a compare-exchange over
//! the lane states, the only read-modify-write in the emitter), and given back by dropping its
//! [`Producer`]; the reader frees a lane it has seen ended, and the next thread to claim it goes
//! on numbering from the lane's head.

// adapted from makepad/relax (MIT), libs/relax/compiler/src/inspect/flightrec.rs (`Slots`: a
// seqlock slot ring overwritten when full); changed: every payload word an atomic, one producer
// a ring, the head stored after the stamp so a drain never meets a slot still being written,
// and lost entries reported by number.

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, fence};

/// The words of a cache line, and of the widest slot.
pub const LINE: usize = 8;
/// The most payload words a slot has.
pub const MOST_PAYLOAD: usize = LINE - 1;
/// The most atomic operations of one push: busy stamp, fence, payload, stamp, head.
pub const MOST_PUSH_OPS: usize = 4 + MOST_PAYLOAD;
/// The most lanes an emitter may have (a thread each).
pub const MOST_LANES: usize = 4096;
/// The most entries a ring may hold.
pub const MOST_LEN: u64 = 1 << 20;

/// The odd bit of a stamp: the slot is being written.
const BUSY: u64 = 1;

/// Entry `n`'s stamp once written.
pub const fn stamp(n: u64) -> u64 {
    (n + 1) << 1
}

/// That `W` words with `P` of payload is a slot: `W` divides a line and `P` is `W - 1`.
const fn shape<const W: usize, const P: usize>() {
    assert!(W >= 2 && LINE.is_multiple_of(W), "a slot is 2, 4 or 8 words");
    assert!(P + 1 == W, "a slot's payload is its words but the stamp");
}

/// A test-only switch that plants a known bug in the protocol, for the oracles' controls (rule
/// 53). Always compiled, so a plant is a value the simulation passes, not a build.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The drain keeps a slot without checking its stamp again after the copy: a torn entry is
    /// delivered.
    NoRecheck,
    /// The drain counts one more lost entry than were overwritten when a ring was lapped.
    LostOffByOne,
    /// A producer waits for the reader before it overwrites a slot: a push blocks.
    WaitForReader,
}

/// One atomic operation of the protocol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Store { at: usize, value: u64, order: Ordering },
    Fence(Ordering),
    /// Only a plant makes this: wait until the word at `at` is at least `least`.
    Await { at: usize, least: u64 },
}

/// The memory the rings are in, by word index. The real one is [`Lines`]; a simulation's runs
/// every operation in an order it chooses.
pub trait Cells {
    fn load(&self, at: usize, order: Ordering) -> u64;
    fn store(&self, at: usize, value: u64, order: Ordering);
    fn fence(&self, order: Ordering);

    /// Carries out `op`; false only for an [`Op::Await`] not yet met, which does nothing.
    fn apply(&self, op: Op) -> bool {
        match op {
            Op::Store { at, value, order } => {
                self.store(at, value, order);
                true
            }
            Op::Fence(order) => {
                self.fence(order);
                true
            }
            Op::Await { at, least } => self.load(at, Ordering::Acquire) >= least,
        }
    }
}

/// One cache line of atomic words.
#[repr(C, align(64))]
pub struct Line(pub [AtomicU64; LINE]);

impl Line {
    pub const fn new() -> Line {
        Line([const { AtomicU64::new(0) }; LINE])
    }
}

impl Default for Line {
    fn default() -> Line {
        Line::new()
    }
}

/// Lines of atomic words in one allocation: the real [`Cells`].
pub struct Lines<A: Allocator> {
    lines: Vec<Line, A>,
}

impl<A: Allocator> Lines<A> {
    pub fn new_in(count: usize, alloc: A) -> Lines<A> {
        let mut lines = Vec::with_capacity_in(count, alloc);
        for _ in 0..count {
            lines.push(Line::new());
        }
        Lines { lines }
    }

    /// The bytes the lines take.
    pub fn bytes(&self) -> usize {
        self.lines.len() * size_of::<Line>()
    }

    /// The line word `at` is in, and its place there.
    #[inline(always)]
    fn word(&self, at: usize) -> &AtomicU64 {
        &self.lines[at / LINE].0[at % LINE]
    }
}

impl<A: Allocator> Cells for Lines<A> {
    #[inline]
    fn load(&self, at: usize, order: Ordering) -> u64 {
        self.word(at).load(order)
    }

    #[inline]
    fn store(&self, at: usize, value: u64, order: Ordering) {
        self.word(at).store(value, order);
    }

    #[inline]
    fn fence(&self, order: Ordering) {
        fence(order);
    }
}

/// Where one ring of `W`-word slots is in the cells: its head (a slot's worth of words, the head
/// word first), then `len` slots, rounded up to whole lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring<const W: usize> {
    pub base: usize,
    pub len: u64,
}

impl<const W: usize> Ring<W> {
    /// The word of the ring's head.
    pub const fn head(self) -> usize {
        self.base
    }

    /// A word only a plant uses: the reader's cursor, published.
    pub const fn reader(self) -> usize {
        self.base + 1
    }

    /// Word `w` of entry `n`'s slot.
    #[inline(always)]
    pub const fn word(self, n: u64, w: usize) -> usize {
        self.base + W * (1 + (n & (self.len - 1)) as usize) + w
    }

    /// The words a ring of `len` slots takes: its head and its slots, in whole lines.
    pub const fn words(len: u64) -> usize {
        (W * (1 + len as usize)).next_multiple_of(LINE)
    }
}

/// The operations of one push, in order: a list of at most [`MOST_PUSH_OPS`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PushOps {
    pub ops: [Op; MOST_PUSH_OPS],
    pub len: usize,
}

/// The operations of pushing `payload` as entry `n` of `ring`, in order (the module's steps 1 to
/// 5): `P + 4` of them whatever the ring holds, no loop, no wait. What a simulation interleaves;
/// [`Producer::push`] does the same stores directly.
pub fn push_ops<const W: usize, const P: usize>(ring: Ring<W>, n: u64, payload: &[u64; P], plant: Plant) -> PushOps {
    const { shape::<W, P>() };
    let mut ops = [Op::Fence(Ordering::Release); MOST_PUSH_OPS];
    ops[0] = Op::Store { at: ring.word(n, 0), value: stamp(n) | BUSY, order: Ordering::Relaxed };
    if plant == Plant::WaitForReader && n >= ring.len {
        // The plant: do not overwrite an entry the reader has not passed.
        ops[1] = Op::Await { at: ring.reader(), least: n + 1 - ring.len };
    }
    for (w, value) in payload.iter().enumerate() {
        ops[2 + w] = Op::Store { at: ring.word(n, 1 + w), value: *value, order: Ordering::Relaxed };
    }
    ops[2 + P] = Op::Store { at: ring.word(n, 0), value: stamp(n), order: Ordering::Release };
    ops[3 + P] = Op::Store { at: ring.head(), value: n + 1, order: Ordering::Release };
    PushOps { ops, len: P + 4 }
}

/// What a drain found, in entry order: an entry's `P` payload words, or a run of lost entries.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Got<const P: usize> {
    Entry { n: u64, words: [u64; P] },
    /// Entries `from..from + count` were overwritten before they were read.
    Lost { from: u64, count: u64 },
}

/// One step of a drain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step<G> {
    /// One operation done; nothing to report yet.
    Working,
    Got(G),
    /// Every entry below `next` is accounted for; the next drain starts there.
    Done { next: u64 },
}

/// Why a drain cannot run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DrainError {
    /// The cursor is past what the ring ever held: a cursor of another ring, or a bug.
    Ahead { from: u64, head: u64 },
}

impl fmt::Display for DrainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            DrainError::Ahead { from, head } => {
                write!(f, "a ring drained from entry {from}, past its head {head}")
            }
        }
    }
}

impl core::error::Error for DrainError {}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Head,
    Stamp,
    Word(usize),
    Fence,
    Recheck,
    Done,
}

/// A drain of one ring of `W`-word slots (`P` payload words) from a cursor up to the head it
/// finds, one atomic operation a step. Holds no lock, writes nothing the producer reads: the
/// producer never waits on it.
pub struct Drain<const W: usize, const P: usize> {
    ring: Ring<W>,
    from: u64,
    head: u64,
    n: u64,
    first: u64,
    words: [u64; P],
    phase: Phase,
    #[doc(hidden)]
    pub plant: Plant,
}

impl<const W: usize, const P: usize> Drain<W, P> {
    pub fn new(ring: Ring<W>, from: u64) -> Drain<W, P> {
        const { shape::<W, P>() };
        Drain { ring, from, head: 0, n: from, first: 0, words: [0; P], phase: Phase::Head, plant: Plant::None }
    }

    fn advance(&mut self) {
        self.n += 1;
        self.phase = if self.n == self.head { Phase::Done } else { Phase::Stamp };
    }

    pub fn step<C: Cells + ?Sized>(&mut self, cells: &C) -> Result<Step<Got<P>>, DrainError> {
        let ring = self.ring;
        match self.phase {
            Phase::Head => {
                let head = cells.load(ring.head(), Ordering::Acquire);
                if self.from > head {
                    return Err(DrainError::Ahead { from: self.from, head });
                }
                self.head = head;
                self.n = self.from;
                if head - self.from > ring.len {
                    self.n = head - ring.len;
                    self.phase = Phase::Stamp;
                    let mut count = self.n - self.from;
                    if self.plant == Plant::LostOffByOne {
                        count += 1;
                    }
                    return Ok(Step::Got(Got::Lost { from: self.from, count }));
                }
                self.phase = if self.n == head { Phase::Done } else { Phase::Stamp };
                Ok(Step::Working)
            }
            Phase::Stamp => {
                let first = cells.load(ring.word(self.n, 0), Ordering::Acquire);
                if first != stamp(self.n) {
                    // Below the head, so written: a later lap is in the slot, or being written.
                    let from = self.n;
                    self.advance();
                    return Ok(Step::Got(Got::Lost { from, count: 1 }));
                }
                self.first = first;
                self.phase = Phase::Word(0);
                Ok(Step::Working)
            }
            Phase::Word(w) => {
                self.words[w] = cells.load(ring.word(self.n, 1 + w), Ordering::Relaxed);
                if w + 1 < P {
                    self.phase = Phase::Word(w + 1);
                    return Ok(Step::Working);
                }
                if self.plant == Plant::NoRecheck {
                    let n = self.n;
                    self.advance();
                    return Ok(Step::Got(Got::Entry { n, words: self.words }));
                }
                self.phase = Phase::Fence;
                Ok(Step::Working)
            }
            Phase::Fence => {
                cells.fence(Ordering::Acquire);
                self.phase = Phase::Recheck;
                Ok(Step::Working)
            }
            Phase::Recheck => {
                let again = cells.load(ring.word(self.n, 0), Ordering::Relaxed);
                let n = self.n;
                self.advance();
                if again != self.first {
                    return Ok(Step::Got(Got::Lost { from: n, count: 1 }));
                }
                Ok(Step::Got(Got::Entry { n, words: self.words }))
            }
            Phase::Done => Ok(Step::Done { next: self.head }),
        }
    }
}

/// A lane of the emitter: the index of one thread's ring.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Lane(pub u32);

/// What a lane is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LaneState {
    Free,
    /// A thread holds its producer.
    Live,
    /// Its thread ended; the reader frees it once it has drained it.
    Ended,
}

const FREE: u64 = 0;
const LIVE: u64 = 1;
const ENDED: u64 = 2;

/// Why an emitter could not be made, or a lane claimed or freed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EmitterError {
    /// A ring length that is not a power of two in `2..=MOST_LEN`, or a lane count outside
    /// `1..=MOST_LANES`.
    Shape { lanes: usize, len: u64 },
    /// Every lane is held: more threads than the emitter was sized for.
    Full { lanes: usize },
    NotEnded { lane: Lane },
}

impl fmt::Display for EmitterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            EmitterError::Shape { lanes, len } => write!(
                f,
                "an emitter of {lanes} lanes of {len} entries: lanes must be 1 to {MOST_LANES} and entries a power of two, 2 to {MOST_LEN}"
            ),
            EmitterError::Full { lanes } => {
                write!(f, "all {lanes} lanes of the emitter are held by live threads")
            }
            EmitterError::NotEnded { lane } => {
                write!(f, "lane {} was freed while its thread still holds it", lane.0)
            }
        }
    }
}

impl core::error::Error for EmitterError {}

/// The rings of one process: `lanes` rings of `len` slots of `W` words, in one allocation made
/// once (`lanes * Ring::words(len) * 8` bytes, touched only as threads record).
pub struct Emitter<A: Allocator, const W: usize> {
    cells: Lines<A>,
    states: Vec<AtomicU64, A>,
    len: u64,
}

impl<A: Allocator + Clone, const W: usize> Emitter<A, W> {
    pub fn new_in(lanes: usize, len: u64, alloc: A) -> Result<Emitter<A, W>, EmitterError> {
        if !(1..=MOST_LANES).contains(&lanes) || !len.is_power_of_two() || !(2..=MOST_LEN).contains(&len) {
            return Err(EmitterError::Shape { lanes, len });
        }
        let cells = Lines::new_in(lanes * Ring::<W>::words(len) / LINE, alloc.clone());
        let mut states = Vec::with_capacity_in(lanes, alloc);
        for _ in 0..lanes {
            states.push(AtomicU64::new(FREE));
        }
        Ok(Emitter { cells, states, len })
    }
}

impl<A: Allocator, const W: usize> Emitter<A, W> {
    pub fn lanes(&self) -> usize {
        self.states.len()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn cells(&self) -> &Lines<A> {
        &self.cells
    }

    pub fn ring(&self, lane: Lane) -> Ring<W> {
        Ring { base: lane.0 as usize * Ring::<W>::words(self.len), len: self.len }
    }

    /// A lane for the calling thread, once, when it starts. `Acquire` on success: it reads the
    /// reader's `Release` of the lane, which read the last producer's head, so this producer
    /// goes on from that head.
    pub fn claim(&self) -> Result<Producer<'_, A, W>, EmitterError> {
        for (index, state) in self.states.iter().enumerate() {
            if state.compare_exchange(FREE, LIVE, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                let lane = Lane(index as u32);
                let ring = self.ring(lane);
                let next = self.cells.load(ring.head(), Ordering::Relaxed);
                return Ok(Producer { emitter: self, ring, lane, next });
            }
        }
        Err(EmitterError::Full { lanes: self.states.len() })
    }

    pub fn state(&self, lane: Lane) -> LaneState {
        match self.states[lane.0 as usize].load(Ordering::Acquire) {
            FREE => LaneState::Free,
            LIVE => LaneState::Live,
            _ => LaneState::Ended,
        }
    }

    /// Gives an ended lane back for another thread (the reader, after its drain reached the
    /// head it found).
    pub fn free(&self, lane: Lane) -> Result<(), EmitterError> {
        self.states[lane.0 as usize]
            .compare_exchange(ENDED, FREE, Ordering::AcqRel, Ordering::Relaxed)
            .map(|_| ())
            .map_err(|_| EmitterError::NotEnded { lane })
    }

    /// Every entry of `lane` from `from` to the head the drain finds, each to `each` in order;
    /// returns the next cursor. Pass `head - len` (or 0) for what the rings keep.
    pub fn drain<const P: usize>(&self, lane: Lane, from: u64, mut each: impl FnMut(Got<P>)) -> Result<u64, DrainError> {
        let mut drain = Drain::<W, P>::new(self.ring(lane), from);
        loop {
            match drain.step(&self.cells)? {
                Step::Working => {}
                Step::Got(got) => each(got),
                Step::Done { next } => return Ok(next),
            }
        }
    }

    /// The lane's head: how many entries it has had.
    pub fn head(&self, lane: Lane) -> u64 {
        self.cells.load(self.ring(lane).head(), Ordering::Acquire)
    }
}

/// One thread's right to push to its lane: the ring's single producer. Not `Clone`; dropping it
/// ends the lane (a `Release` store, so the reader that sees `Ended` sees every entry before).
pub struct Producer<'e, A: Allocator, const W: usize> {
    emitter: &'e Emitter<A, W>,
    ring: Ring<W>,
    lane: Lane,
    next: u64,
}

impl<A: Allocator, const W: usize> Producer<'_, A, W> {
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// The number the next push gets.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Pushes `payload` as the next entry and returns its number: [`push_ops`]' stores and
    /// fence, done directly, never a wait.
    #[inline(always)]
    pub fn push<const P: usize>(&mut self, payload: &[u64; P]) -> u64 {
        const { shape::<W, P>() };
        let (ring, n, cells) = (self.ring, self.next, &self.emitter.cells);
        let slot = ring.word(n, 0);
        let line = &cells.lines[slot / LINE].0;
        let at = slot % LINE;
        line[at].store(stamp(n) | BUSY, Ordering::Relaxed);
        fence(Ordering::Release);
        for (w, value) in payload.iter().enumerate() {
            line[at + 1 + w].store(*value, Ordering::Relaxed);
        }
        line[at].store(stamp(n), Ordering::Release);
        cells.store(ring.head(), n + 1, Ordering::Release);
        self.next = n + 1;
        n
    }
}

impl<A: Allocator, const W: usize> Drop for Producer<'_, A, W> {
    fn drop(&mut self) {
        self.emitter.states[self.lane.0 as usize].store(ENDED, Ordering::Release);
    }
}
