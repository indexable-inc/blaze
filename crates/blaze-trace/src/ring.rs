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
//! **The protocol** has every word atomic (so no data race exists to reason about), and a fence
//! a block of entries, not a slot. Entry `n` (0-based) is in slot `n mod len`; its stamp is
//! `(n + 1) << 1`. The ring's slots are cut in blocks of [`Ring::block`] entries (64 in a real
//! ring), and the ring's **frontier** word says how far the producer may be writing: every entry
//! at or past it is untouched. A push is the fixed list [`push_ops`]:
//!
//! 1. at a block's first entry only, the frontier, the block's end (`Relaxed`);
//! 2. then only, a `Release` fence: a reader that sees any payload word written after it, and
//!    then fences `Acquire`, sees step 1's frontier or a later one;
//! 3. the payload words and the stamp (`Relaxed`);
//! 4. the ring's head, `n + 1` (`Release`): a reader that loads it `Acquire` sees every entry
//!    below it whole (or overwritten by a later lap, which the frontier says).
//!
//! The producer is the ring's only writer, so the head is a store, not a `fetch_add`: no
//! read-modify-write and no contention. A push is the same operations whatever any reader does:
//! wait-free. When the ring is full the oldest entry is overwritten. Measured
//! (`examples/cost.rs`, 2026-11-15, an arm64 Mac): a fence a slot was 1.2 ns of a 2.6 ns push.
//!
//! Why the fence is still needed, a block: without it the stores of entry `n + len` (the next
//! lap in `n`'s slot) may be seen before anything that says they began, so a reader copying
//! entry `n` could take half of each and keep it. Step 2 orders "this block may be written"
//! before the block's stores; the reader's check after its copy sees it.
//!
//! **A drain** ([`Drain`], a step machine, one atomic operation a step) loads the head
//! (`Acquire`), and for each entry `n` from its cursor: the stamp, the payload (`Relaxed`), an
//! `Acquire` fence, then the frontier (`Relaxed`); the entry is kept only when the stamp is its
//! own and the frontier is at most `n + len` (no later lap has begun in its slot). Entries the
//! producer lapped before the drain reached them, and entries whose block was opened for the
//! next lap during the copy, are reported as [`Got::Lost`] with their numbers, so
//! `delivered + lost` is exactly the number of entries pushed since the cursor, and an entry is
//! lost only once its slot's block was opened for the next lap: only the oldest are lost (at
//! most a block sooner than a slot-at-a-time ring would lose them).
//!
//! **A thread's lane** is claimed once, when the thread first records (a compare-exchange over
//! the lane states, the only read-modify-write in the emitter), and given back by dropping its
//! [`Producer`]; the reader frees a lane it has seen ended, and the next thread to claim it goes
//! on numbering from the lane's head.

// adapted from makepad/relax (MIT), libs/relax/compiler/src/inspect/flightrec.rs (`Slots`: a
// seqlock slot ring overwritten when full); changed: every payload word an atomic, one producer
// a ring, the head stored after the stamp so a drain never meets a slot still being written,
// and lost entries reported by number.

// A core: atomics over memory it was handed (`Lines` over a named allocator, or a simulation's
// `Cells`), no clock, no IO; the drain is a step machine a simulation interleaves.
#![blaze::sans_io]

use core::alloc::Allocator;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use blaze_own::Vec;
use blaze_own::planted::{NoPlant, Planted, Plants};

/// The words of a cache line, and of the widest slot.
pub const LINE: usize = 8;
/// The most payload words a slot has.
pub const MOST_PAYLOAD: usize = LINE - 1;
/// The most atomic operations of one push: frontier, fence, a plant's wait, payload, stamp,
/// head.
pub const MOST_PUSH_OPS: usize = 5 + MOST_PAYLOAD;
/// The most lanes an emitter may have (a thread each): [`EmitterLimits::SHIPPED`]'s.
pub const MOST_LANES: usize = 4096;
/// The most entries a ring may hold: [`EmitterLimits::SHIPPED`]'s.
pub const MOST_LEN: u64 = 1 << 20;

/// An emitter's caps as one value (D165): the shipped values are [`EmitterLimits::SHIPPED`], and
/// a simulation draws each down to its smallest legal value ([`Emitter::new_limited_in`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct EmitterLimits {
    /// The most lanes (threads) an emitter is made with, at least 1.
    pub most_lanes: usize,
    /// The most entries a ring is made with, a power of two, at least 2.
    pub most_len: u64,
}

impl EmitterLimits {
    /// The caps shipped: [`MOST_LANES`] lanes of at most [`MOST_LEN`] entries.
    pub const SHIPPED: EmitterLimits = EmitterLimits { most_lanes: MOST_LANES, most_len: MOST_LEN };
}

/// Entry `n`'s stamp.
#[inline(always)]
pub const fn stamp(n: u64) -> u64 {
    (n + 1) << 1
}

/// The frontier's word in a ring's head slot (the head is word 0, the plant's reader word 1).
const FRONTIER: usize = 2;

/// The most entries of a block: one frontier store and one fence each.
pub const MOST_BLOCK: u64 = 64;

/// That `W` words with `P` of payload is a slot: `W` divides a line and `P` is `W - 1`; the
/// head's slot holds the head, the reader's word and the frontier.
const fn shape<const W: usize, const P: usize>() {
    assert!(W >= 4 && LINE.is_multiple_of(W), "a slot is 4 or 8 words");
    assert!(P + 1 == W, "a slot's payload is its words but the stamp");
}

/// The bugs a control plants in the protocol, for the oracles' controls (rule 53, D165): taken
/// through the [`Planted`] seam ([`push_ops`]' `plant`, [`Drain`]'s `S`), with [`NoPlant`] in
/// shipped code ([`Emitter::drain`]), where every check compiles to `false`.
#[doc(hidden)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Plant {
    None,
    /// The drain keeps a slot without checking the frontier after the copy: a torn entry is
    /// delivered.
    NoRecheck,
    /// The drain counts one more lost entry than were overwritten when a ring was lapped.
    LostOffByOne,
    /// A producer waits for the reader before it overwrites a slot: a push blocks.
    WaitForReader,
}

impl Plants for Plant {}

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

    /// Every word, in order.
    fn all(&self) -> &[AtomicU64] {
        // SAFETY: `Line` is `repr(C)`, 8 words of 8 bytes aligned to 64: no padding, so `count`
        // lines are `count * LINE` words in a row, borrowed for as long as `self` is.
        unsafe { core::slice::from_raw_parts(self.lines.as_ptr().cast::<AtomicU64>(), self.lines.len() * LINE) }
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

    /// The word of the ring's frontier: every entry at or past it is untouched.
    pub const fn frontier(self) -> usize {
        self.base + FRONTIER
    }

    /// The entries of a block: an eighth of the ring, 1 to [`MOST_BLOCK`], a power of two.
    #[inline(always)]
    pub const fn block(self) -> u64 {
        let block = self.len / 8;
        if block == 0 {
            1
        } else if block > MOST_BLOCK {
            MOST_BLOCK
        } else {
            block
        }
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
pub fn push_ops<const W: usize, const P: usize>(
    ring: Ring<W>,
    n: u64,
    payload: &[u64; P],
    plant: impl Planted<Plant>,
) -> PushOps {
    const { shape::<W, P>() };
    // The fillers are fences, which order nothing more than the protocol's own operations do.
    let mut ops = [Op::Fence(Ordering::Release); MOST_PUSH_OPS];
    let block = ring.block();
    if n % block == 0 {
        ops[0] = Op::Store { at: ring.frontier(), value: n + block, order: Ordering::Relaxed };
    }
    if plant.has(Plant::WaitForReader) && n >= ring.len {
        // The plant: do not overwrite an entry the reader has not passed.
        ops[2] = Op::Await { at: ring.reader(), least: n + 1 - ring.len };
    }
    for (w, value) in payload.iter().enumerate() {
        ops[3 + w] = Op::Store { at: ring.word(n, 1 + w), value: *value, order: Ordering::Relaxed };
    }
    ops[3 + P] = Op::Store { at: ring.word(n, 0), value: stamp(n), order: Ordering::Relaxed };
    ops[4 + P] = Op::Store { at: ring.head(), value: n + 1, order: Ordering::Release };
    PushOps { ops, len: P + 5 }
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
///
/// `S` is the plant seam: [`NoPlant`] in shipped code ([`Drain::unplanted`], [`Emitter::drain`]),
/// a [`Plant`] in a control. The default `S = Plant` is kept only so [`Drain::new`]'s callers
/// (blaze-watch's ring.rs, which sets `plant` before each step) compile unchanged; it goes once
/// they name `S`.
pub struct Drain<const W: usize, const P: usize, S: Planted<Plant> = Plant> {
    ring: Ring<W>,
    from: u64,
    head: u64,
    n: u64,
    words: [u64; P],
    phase: Phase,
    #[doc(hidden)]
    pub plant: S,
}

impl<const W: usize, const P: usize> Drain<W, P, Plant> {
    /// A drain whose plant is a value set on it (`plant`, [`Plant::None`] here).
    ///
    /// Deprecated: shipped code takes [`Drain::unplanted`], a control [`Drain::planted`].
    /// Kept for blaze-watch's ring.rs (`base::Drain::new`), which sets `plant` before a step.
    pub fn new(ring: Ring<W>, from: u64) -> Drain<W, P, Plant> {
        Drain::planted(ring, from, Plant::None)
    }
}

impl<const W: usize, const P: usize> Drain<W, P, NoPlant> {
    /// A drain with no plant: every plant check compiles to `false`.
    pub fn unplanted(ring: Ring<W>, from: u64) -> Drain<W, P, NoPlant> {
        Drain::planted(ring, from, NoPlant)
    }
}

impl<const W: usize, const P: usize, S: Planted<Plant>> Drain<W, P, S> {
    /// A drain taking its plant through the seam.
    pub fn planted(ring: Ring<W>, from: u64, plant: S) -> Drain<W, P, S> {
        const { shape::<W, P>() };
        Drain { ring, from, head: 0, n: from, words: [0; P], phase: Phase::Head, plant }
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
                    if self.plant.has(Plant::LostOffByOne) {
                        count += 1;
                    }
                    return Ok(Step::Got(Got::Lost { from: self.from, count }));
                }
                self.phase = if self.n == head { Phase::Done } else { Phase::Stamp };
                Ok(Step::Working)
            }
            Phase::Stamp => {
                // Below the head, so written: another stamp is a later lap's.
                if cells.load(ring.word(self.n, 0), Ordering::Relaxed) != stamp(self.n) {
                    let from = self.n;
                    self.advance();
                    return Ok(Step::Got(Got::Lost { from, count: 1 }));
                }
                self.phase = Phase::Word(0);
                Ok(Step::Working)
            }
            Phase::Word(w) => {
                self.words[w] = cells.load(ring.word(self.n, 1 + w), Ordering::Relaxed);
                if w + 1 < P {
                    self.phase = Phase::Word(w + 1);
                    return Ok(Step::Working);
                }
                if self.plant.has(Plant::NoRecheck) {
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
                // After the fence: a later lap that began in this slot opened its block first.
                let frontier = cells.load(ring.frontier(), Ordering::Relaxed);
                let n = self.n;
                self.advance();
                if frontier > n + ring.len {
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
    /// `lanes` rings of `len` entries from `alloc`, under the shipped caps.
    pub fn new_in(lanes: usize, len: u64, alloc: A) -> Result<Emitter<A, W>, EmitterError> {
        Emitter::new_limited_in(lanes, len, EmitterLimits::SHIPPED, alloc)
    }

    /// `lanes` rings of `len` entries from `alloc`, under `limits` (a simulation's drawn caps).
    pub fn new_limited_in(lanes: usize, len: u64, limits: EmitterLimits, alloc: A) -> Result<Emitter<A, W>, EmitterError> {
        if !(1..=limits.most_lanes).contains(&lanes) || !len.is_power_of_two() || !(2..=limits.most_len).contains(&len) {
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
                let words = &self.cells.all()[ring.base..ring.base + Ring::<W>::words(self.len)];
                // The first push opens its block whatever its number: the last producer's
                // frontier was its own thread's.
                return Ok(Producer { emitter: self, words, mask: self.len - 1, block: ring.block(), lane, next, open: next });
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
        let mut drain = Drain::<W, P, NoPlant>::unplanted(self.ring(lane), from);
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
    /// The ring's own words: its head slot, then its `len` slots.
    words: &'e [AtomicU64],
    /// `len - 1`: a slot's number from an entry's.
    mask: u64,
    block: u64,
    lane: Lane,
    next: u64,
    /// The entry whose push opens a block next.
    open: u64,
}

impl<A: Allocator, const W: usize> Producer<'_, A, W> {
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// The number the next push gets.
    pub fn next(&self) -> u64 {
        self.next
    }

    /// Pushes `payload` as the next entry and returns its number: [`push_ops`]' stores (and, a
    /// block, its fence), done directly, never a wait.
    #[inline(always)]
    pub fn push<const P: usize>(&mut self, payload: &[u64; P]) -> u64 {
        const { shape::<W, P>() };
        let n = self.next;
        if n == self.open {
            self.open_block(n);
        }
        // In locals: the stores below may not be assumed apart from `self`, which would be read
        // again after each.
        let words = self.words;
        let at = W * (1 + (n & self.mask) as usize);
        // SAFETY: `words` is the ring's `Ring::words(len)` words, at least `W * (1 + len)`, and
        // `at + W <= W * (1 + len)` because `n & mask < len`; the head is word 0.
        unsafe {
            for (w, value) in payload.iter().enumerate() {
                words.get_unchecked(at + 1 + w).store(*value, Ordering::Relaxed);
            }
            words.get_unchecked(at).store(stamp(n), Ordering::Relaxed);
            words.get_unchecked(0).store(n + 1, Ordering::Release);
        }
        self.next = n + 1;
        n
    }

    /// Steps 1 and 2: the frontier moves to the end of `n`'s block, then the fence.
    #[inline(never)]
    fn open_block(&mut self, n: u64) {
        let end = (n | (self.block - 1)) + 1;
        self.words[FRONTIER].store(end, Ordering::Relaxed);
        fence(Ordering::Release);
        self.open = end;
    }
}

impl<A: Allocator, const W: usize> Drop for Producer<'_, A, W> {
    fn drop(&mut self) {
        self.emitter.states[self.lane.0 as usize].store(ENDED, Ordering::Release);
    }
}
