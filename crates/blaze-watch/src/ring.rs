//! The emitter: one single-producer ring of fixed-size entries a thread, in one fixed table
//! ([`Emitter`]), so an app thread emits an event without a lock, a wait or a read-modify-write
//! (the 128-core rule), and a reader drains every ring without ever holding a producer back.
//!
//! The rings are blaze-trace's (`blaze_trace::ring`, their one home: the protocol, the drain
//! and the lanes are documented there); this module is their 64-byte width carrying an
//! [`Event`]: a stamp word and seven payload words (kind, flags, detail, time, value and a
//! 32-byte key). A push is twelve operations ([`push_ops`]), fences filling the ones only a
//! block's first push needs.
//!
//! **The flight recorder** is these rings, read from `head - len` without moving any cursor
//! ([`Emitter::drain`] takes the cursor it is given): the last `len` entries of every thread are
//! always there. [`crate::recorder`] decides when to save them.
//!
//! Where the producer lives in a thread is the driver's (bstd has no thread-local storage: the
//! thread's `Cx` holds it).

use core::alloc::Allocator;

use blaze_trace::ring as base;
pub use blaze_trace::ring::{Cells, DrainError, EmitterError, Lane, LaneState, Line, Lines, MOST_LANES, MOST_LEN, Op, stamp};

use crate::Plant;

/// The words of one slot: the stamp and the payload.
pub const WORDS: usize = 8;
/// The payload words of an entry.
pub const PAYLOAD: usize = WORDS - 1;
/// The atomic operations of one push: frontier, fence, a plant's wait, payload, stamp, head.
pub const PUSH_OPS: usize = 5 + PAYLOAD;

/// Where one ring of events is in the cells.
pub type Ring = base::Ring<WORDS>;

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

/// The ring's plant for a machine's.
fn ring_plant(plant: Plant) -> base::Plant {
    match plant {
        Plant::NoRecheck => base::Plant::NoRecheck,
        Plant::LostOffByOne => base::Plant::LostOffByOne,
        Plant::WaitForReader => base::Plant::WaitForReader,
        _ => base::Plant::None,
    }
}

/// The operations of pushing `event` as entry `n` of `ring`, in order. The same twelve
/// whatever the ring holds: no loop, no wait.
pub fn push_ops(ring: Ring, n: u64, event: &Event) -> [Op; PUSH_OPS] {
    push_ops_planted(ring, n, event, Plant::None)
}

pub(crate) fn push_ops_planted(ring: Ring, n: u64, event: &Event, plant: Plant) -> [Op; PUSH_OPS] {
    let pushed = base::push_ops(ring, n, &event.words(), ring_plant(plant));
    let mut ops = [Op::Fence(core::sync::atomic::Ordering::Release); PUSH_OPS];
    ops.copy_from_slice(&pushed.ops[..PUSH_OPS]);
    ops
}

/// What a drain found, in entry order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Got {
    Event { n: u64, event: Event },
    /// Entries `from..from + count` were overwritten before they were read.
    Lost { from: u64, count: u64 },
}

impl From<base::Got<PAYLOAD>> for Got {
    fn from(got: base::Got<PAYLOAD>) -> Got {
        match got {
            base::Got::Entry { n, words } => Got::Event { n, event: Event::from_words(&words) },
            base::Got::Lost { from, count } => Got::Lost { from, count },
        }
    }
}

/// One step of a drain.
pub type Step = base::Step<Got>;

/// A drain of one ring from a cursor up to the head it finds, one atomic operation a step.
pub struct Drain {
    inner: base::Drain<WORDS, PAYLOAD>,
    pub(crate) plant: Plant,
}

impl Drain {
    pub fn new(ring: Ring, from: u64) -> Drain {
        Drain { inner: base::Drain::new(ring, from), plant: Plant::None }
    }

    pub fn step<C: Cells + ?Sized>(&mut self, cells: &C) -> Result<Step, DrainError> {
        self.inner.plant = ring_plant(self.plant);
        Ok(match self.inner.step(cells)? {
            base::Step::Working => Step::Working,
            base::Step::Got(got) => Step::Got(got.into()),
            base::Step::Done { next } => Step::Done { next },
        })
    }
}

/// The rings of one process: `lanes` rings of `len` entries, in one allocation of
/// `lanes * (len + 1) * 64` bytes, made once (1024 entries and 256 lanes: 16.8 MB of address
/// space, touched only as threads emit).
pub struct Emitter<A: Allocator>(base::Emitter<A, WORDS>);

impl<A: Allocator + Clone> Emitter<A> {
    pub fn new_in(lanes: usize, len: u64, alloc: A) -> Result<Emitter<A>, EmitterError> {
        base::Emitter::new_in(lanes, len, alloc).map(Emitter)
    }
}

impl<A: Allocator> Emitter<A> {
    pub fn lanes(&self) -> usize {
        self.0.lanes()
    }

    pub fn len(&self) -> u64 {
        self.0.len()
    }

    pub fn cells(&self) -> &Lines<A> {
        self.0.cells()
    }

    pub fn ring(&self, lane: Lane) -> Ring {
        self.0.ring(lane)
    }

    /// A lane for the calling thread, once, when it starts.
    pub fn claim(&self) -> Result<Producer<'_, A>, EmitterError> {
        self.0.claim().map(Producer)
    }

    pub fn state(&self, lane: Lane) -> LaneState {
        self.0.state(lane)
    }

    /// Gives an ended lane back for another thread (the reader, after its drain reached the
    /// head it found).
    pub fn free(&self, lane: Lane) -> Result<(), EmitterError> {
        self.0.free(lane)
    }

    /// Every entry of `lane` from `from` to the head the drain finds, each to `each` in order;
    /// returns the next cursor. Pass `head - len` (or 0) for what the recorder keeps.
    pub fn drain(&self, lane: Lane, from: u64, mut each: impl FnMut(Got)) -> Result<u64, DrainError> {
        self.0.drain::<PAYLOAD>(lane, from, |got| each(got.into()))
    }

    /// The lane's head: how many entries it has had.
    pub fn head(&self, lane: Lane) -> u64 {
        self.0.head(lane)
    }
}

/// One thread's right to push to its lane: the ring's single producer. Dropping it ends the
/// lane.
pub struct Producer<'e, A: Allocator>(base::Producer<'e, A, WORDS>);

impl<A: Allocator> Producer<'_, A> {
    pub fn lane(&self) -> Lane {
        self.0.lane()
    }

    /// Emits `event`: nine stores (a block's first: one more and a fence), never a wait.
    #[inline]
    pub fn push(&mut self, event: &Event) {
        self.0.push(&event.words());
    }
}
