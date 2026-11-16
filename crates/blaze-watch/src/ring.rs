//! The emitter: one single-producer ring of fixed-size entries a thread, in one fixed table
//! ([`Emitter`]), so an app thread emits an event without a lock, a wait or a read-modify-write
//! (the 128-core rule), and a reader drains every ring without ever holding a producer back.
//!
//! **An entry** is one 64-byte line: a stamp word and seven payload words ([`Event`]: kind,
//! flags, detail, time, value and a 32-byte key). A slot's line is its own, so two producers
//! never share a line and a producer shares one with the reader only on the slot it writes.
//!
//! **The protocol** is a seqlock with every word atomic (so no data race exists to reason about,
//! and no `unsafe`): Boehm's form ("Can seqlocks get along with programming language memory
//! models?", 2012). Entry `n` (0-based) is in slot `n mod len`; its stamp is `(n + 1) << 1`, odd
//! while it is written. A push is the fixed list [`push_ops`]:
//!
//! 1. the stamp, odd (`Relaxed`): marks the slot busy;
//! 2. a `Release` fence: a reader that sees any payload word of this write, and then fences
//!    `Acquire`, sees step 1 or later in its second stamp load, so it cannot accept a torn copy;
//! 3. the seven payload words (`Relaxed`);
//! 4. the stamp, even (`Release`): a reader that loads it `Acquire` sees the whole payload;
//! 5. the ring's head, `n + 1` (`Release`): a reader that loads it `Acquire` knows every entry
//!    below is stamped (or overwritten by a later lap, which its stamp says).
//!
//! The producer is the ring's only writer, so the head is a store, not a `fetch_add`: no
//! read-modify-write and no contention. A push is the same eleven operations whatever any
//! reader does: wait-free. When the ring is full the oldest entry is overwritten.
//!
//! **A drain** ([`Drain`], a step machine, one atomic operation a step) loads the head
//! (`Acquire`), and for each entry from its cursor: the stamp (`Acquire`), the payload
//! (`Relaxed`), an `Acquire` fence, the stamp again (`Relaxed`); the entry is kept only when
//! both stamps are its own even stamp. Entries the producer lapped before the drain reached
//! them, and entries overwritten during the copy, are reported as [`Got::Lost`] with their
//! numbers, so `delivered + lost` is exactly the number of entries pushed since the cursor, and
//! an entry is lost only when `len` newer ones were pushed: only the oldest are lost.
//!
//! **The flight recorder** is these rings, read from `head - len` without moving any cursor
//! ([`Emitter::drain`] takes the cursor it is given): the last `len` entries of every thread are
//! always there. [`crate::recorder`] decides when to save them.
//!
//! **A thread's lane** is claimed once, when the thread starts (a compare-exchange over the
//! lane states, the only read-modify-write in the emitter), and given back by dropping its
//! [`Producer`]; the reader frees a lane it has seen ended, and the next thread to claim it
//! goes on numbering from the lane's head. Where the producer lives in a thread is the
//! driver's (bstd has no thread-local storage: the thread's `Cx` holds it).

// adapted from makepad/relax (MIT), libs/relax/compiler/src/inspect/flightrec.rs (`Slots`: a
// seqlock slot ring overwritten when full); changed: every payload word an atomic, one producer
// a ring, the head stored after the stamp so a drain never meets a slot still being written,
// and lost entries reported by number.

use core::alloc::Allocator;
use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering, fence};

use blaze_own::Vec;

use crate::Plant;

/// The words of one slot: the stamp and the payload.
pub const WORDS: usize = 8;
/// The payload words of an entry.
pub const PAYLOAD: usize = WORDS - 1;
/// The atomic operations of one push: busy stamp, fence, payload, stamp, head.
pub const PUSH_OPS: usize = 4 + PAYLOAD;
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

/// One event as a thread emits it: plain data, keys and numbers only.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Event {
    /// What happened ([`kind`]).
    pub kind: u16,
    pub flags: u16,
    /// A detail the kind defines (a wait's class, a test's index), never a pointer.
    pub detail: u32,
    pub at: u64,
    pub value: u64,
    /// The function, test, program or record the event is about; zeros for none.
    pub key: [u8; 32],
}

/// The kinds of [`Event`]. 0 to 8 are blaze-run's `RunEvent` kinds, by the same numbers as its
/// `events` batches.
pub mod kind {
    pub const BEGAN: u16 = 0;
    pub const COMPILING: u16 = 1;
    pub const PUBLISHED: u16 = 2;
    pub const TIERED: u16 = 3;
    pub const FAILED: u16 = 4;
    pub const TEST_PASSED: u16 = 5;
    pub const TEST_FAILED: u16 = 6;
    pub const MEMORY: u16 = 7;
    pub const ENDED: u16 = 8;
    /// A thread began a blocking wait (rule 15): `detail` its class, `value` a wait id.
    pub const WAIT_BEGAN: u16 = 16;
    /// The wait of id `value` ended.
    pub const WAIT_ENDED: u16 = 17;
    /// A job panicked (rule 20): `key` the job.
    pub const PANIC: u16 = 18;
    /// A span began: `detail` its name id.
    pub const SPAN_BEGAN: u16 = 19;
    pub const SPAN_ENDED: u16 = 20;
    /// One effect through `Io` (bottom.md section 14): `detail` its kind, `value` its size.
    pub const EFFECT: u16 = 21;
    /// A stack sample of the thread in `detail`, taken by the sampler: `key` names the record.
    pub const SAMPLE: u16 = 22;
}

impl Event {
    /// The seven payload words.
    pub fn words(&self) -> [u64; PAYLOAD] {
        let mut out = [0u64; PAYLOAD];
        out[0] = u64::from(self.kind) | (u64::from(self.flags) << 16) | (u64::from(self.detail) << 32);
        out[1] = self.at;
        out[2] = self.value;
        for (i, word) in out[3..].iter_mut().enumerate() {
            let mut chunk = [0u8; 8];
            chunk.copy_from_slice(&self.key[i * 8..i * 8 + 8]);
            *word = u64::from_le_bytes(chunk);
        }
        out
    }

    pub fn from_words(words: &[u64; PAYLOAD]) -> Event {
        let mut key = [0u8; 32];
        for (i, word) in words[3..].iter().enumerate() {
            key[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        Event {
            kind: words[0] as u16,
            flags: (words[0] >> 16) as u16,
            detail: (words[0] >> 32) as u32,
            at: words[1],
            value: words[2],
            key,
        }
    }
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
pub struct Line(pub [AtomicU64; WORDS]);

impl Line {
    pub const fn new() -> Line {
        Line([const { AtomicU64::new(0) }; WORDS])
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
}

impl<A: Allocator> Cells for Lines<A> {
    #[inline]
    fn load(&self, at: usize, order: Ordering) -> u64 {
        self.lines[at / WORDS].0[at % WORDS].load(order)
    }

    #[inline]
    fn store(&self, at: usize, value: u64, order: Ordering) {
        self.lines[at / WORDS].0[at % WORDS].store(value, order);
    }

    #[inline]
    fn fence(&self, order: Ordering) {
        fence(order);
    }
}

/// Where one ring is in the cells: its head line, then `len` slot lines.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ring {
    pub base: usize,
    pub len: u64,
}

impl Ring {
    /// The word of the ring's head.
    pub const fn head(self) -> usize {
        self.base
    }

    /// A word only a plant uses: the reader's cursor, published.
    pub const fn reader(self) -> usize {
        self.base + 1
    }

    /// Word `w` of entry `n`'s slot.
    pub const fn word(self, n: u64, w: usize) -> usize {
        self.base + WORDS * (1 + (n & (self.len - 1)) as usize) + w
    }

    /// The words the ring takes: its head line and its slots.
    pub const fn words(len: u64) -> usize {
        WORDS * (1 + len as usize)
    }
}

/// The operations of pushing `event` as entry `n` of `ring`, in order (the module's steps 1 to
/// 5). The same eleven whatever the ring holds: no loop, no wait.
pub fn push_ops(ring: Ring, n: u64, event: &Event) -> [Op; PUSH_OPS] {
    push_ops_planted(ring, n, event, Plant::None)
}

pub(crate) fn push_ops_planted(ring: Ring, n: u64, event: &Event, plant: Plant) -> [Op; PUSH_OPS] {
    let words = event.words();
    let mut ops = [Op::Fence(Ordering::Release); PUSH_OPS];
    ops[0] = Op::Store { at: ring.word(n, 0), value: stamp(n) | BUSY, order: Ordering::Relaxed };
    if plant == Plant::WaitForReader && n >= ring.len {
        // The plant: do not overwrite an entry the reader has not passed.
        ops[1] = Op::Await { at: ring.reader(), least: n + 1 - ring.len };
    }
    for (w, value) in words.iter().enumerate() {
        ops[2 + w] = Op::Store { at: ring.word(n, 1 + w), value: *value, order: Ordering::Relaxed };
    }
    ops[2 + PAYLOAD] = Op::Store { at: ring.word(n, 0), value: stamp(n), order: Ordering::Release };
    ops[3 + PAYLOAD] = Op::Store { at: ring.head(), value: n + 1, order: Ordering::Release };
    ops
}

/// What a drain found, in entry order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Got {
    Event { n: u64, event: Event },
    /// Entries `from..from + count` were overwritten before they were read.
    Lost { from: u64, count: u64 },
}

/// One step of a drain.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Step {
    /// One operation done; nothing to report yet.
    Working,
    Got(Got),
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

/// A drain of one ring from a cursor up to the head it finds, one atomic operation a step.
/// Holds no lock, writes nothing the producer reads: the producer never waits on it.
pub struct Drain {
    ring: Ring,
    from: u64,
    head: u64,
    n: u64,
    first: u64,
    words: [u64; PAYLOAD],
    phase: Phase,
    pub(crate) plant: Plant,
}

impl Drain {
    pub fn new(ring: Ring, from: u64) -> Drain {
        Drain {
            ring,
            from,
            head: 0,
            n: from,
            first: 0,
            words: [0; PAYLOAD],
            phase: Phase::Head,
            plant: Plant::None,
        }
    }

    fn advance(&mut self) {
        self.n += 1;
        self.phase = if self.n == self.head { Phase::Done } else { Phase::Stamp };
    }

    pub fn step<C: Cells + ?Sized>(&mut self, cells: &C) -> Result<Step, DrainError> {
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
                if w + 1 < PAYLOAD {
                    self.phase = Phase::Word(w + 1);
                    return Ok(Step::Working);
                }
                if self.plant == Plant::NoRecheck {
                    let n = self.n;
                    self.advance();
                    return Ok(Step::Got(Got::Event { n, event: Event::from_words(&self.words) }));
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
                Ok(Step::Got(Got::Event { n, event: Event::from_words(&self.words) }))
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

/// The rings of one process: `lanes` rings of `len` entries, in one allocation of
/// `lanes * (len + 1) * 64` bytes, made once (1024 entries and 256 lanes: 16.8 MB of address
/// space, touched only as threads emit).
pub struct Emitter<A: Allocator> {
    cells: Lines<A>,
    states: Vec<AtomicU64, A>,
    len: u64,
}

impl<A: Allocator + Clone> Emitter<A> {
    pub fn new_in(lanes: usize, len: u64, alloc: A) -> Result<Emitter<A>, EmitterError> {
        if !(1..=MOST_LANES).contains(&lanes) || !len.is_power_of_two() || !(2..=MOST_LEN).contains(&len) {
            return Err(EmitterError::Shape { lanes, len });
        }
        let cells = Lines::new_in(lanes * (1 + len as usize), alloc.clone());
        let mut states = Vec::with_capacity_in(lanes, alloc);
        for _ in 0..lanes {
            states.push(AtomicU64::new(FREE));
        }
        Ok(Emitter { cells, states, len })
    }
}

impl<A: Allocator> Emitter<A> {
    pub fn lanes(&self) -> usize {
        self.states.len()
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn cells(&self) -> &Lines<A> {
        &self.cells
    }

    pub fn ring(&self, lane: Lane) -> Ring {
        Ring { base: lane.0 as usize * Ring::words(self.len), len: self.len }
    }

    /// A lane for the calling thread, once, when it starts. `Acquire` on success: it reads the
    /// reader's `Release` of the lane, which read the last producer's head, so this producer
    /// goes on from that head.
    pub fn claim(&self) -> Result<Producer<'_, A>, EmitterError> {
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
    /// returns the next cursor. Pass `head - len` (or 0) for what the recorder keeps.
    pub fn drain(&self, lane: Lane, from: u64, mut each: impl FnMut(Got)) -> Result<u64, DrainError> {
        let mut drain = Drain::new(self.ring(lane), from);
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
pub struct Producer<'e, A: Allocator> {
    emitter: &'e Emitter<A>,
    ring: Ring,
    lane: Lane,
    next: u64,
}

impl<A: Allocator> Producer<'_, A> {
    pub fn lane(&self) -> Lane {
        self.lane
    }

    /// Emits `event`: eleven stores and a fence, never a wait.
    #[inline]
    pub fn push(&mut self, event: &Event) {
        for op in push_ops(self.ring, self.next, event) {
            self.emitter.cells.apply(op);
        }
        self.next += 1;
    }
}

impl<A: Allocator> Drop for Producer<'_, A> {
    fn drop(&mut self) {
        self.emitter.states[self.lane.0 as usize].store(ENDED, Ordering::Release);
    }
}
