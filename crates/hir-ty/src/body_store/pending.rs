//! Offered steps, buffered until [`super::Installed::flush`]: one buffer per shard of threads,
//! each its own lock and cache line, so an offer takes an uncontended lock and appends, and no
//! worker waits on the store.
//!
//! An entry is its length, its step, three pad bytes, its shape's length, its answer's length,
//! its base key, its shape (`record`'s bytes, as stored), its facts' values (32 bytes each, in
//! the shape's order) and its answer (the step's result); its body is at the same index in the
//! shard's `defs`, for the errors that name it.

use std::{
    collections::hash_map::Entry as Slot,
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use hir_def::DefWithBodyId;
use parking_lot::{Mutex, MutexGuard};

use super::{Refusal, Step, StepCounts};

const SHARDS: usize = 64;

/// The most a shard buffers between flushes: past it an offer is dropped and counted
/// ([`super::Flushed::dropped`]), so a build that offers more than a flush writes keeps its
/// memory bounded. 256 MiB in all: an inference answer is a few KiB.
const SHARD_BYTES: usize = 4 << 20;

/// Entry length, step and pad, shape length, answer length, base key.
const ENTRY_HEAD: usize = 4 + 4 + 4 + 4 + 32;

pub(super) struct Pending {
    shards: Box<[Padded]>,
}

#[repr(align(128))]
#[derive(Default)]
struct Padded(Mutex<Shard>);

#[derive(Default)]
struct Shard {
    bytes: Vec<u8>,
    defs: Vec<DefWithBodyId>,
    tallies: Counts,
    refused: HashMap<&'static str, u64>,
}

/// Why offered steps were not kept, by every reason but a read, and what each step's lookups
/// found.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) unhashable: u64,
    pub(super) no_identity: u64,
    pub(super) unshareable: u64,
    pub(super) unencodable: u64,
    pub(super) dropped: u64,
    /// By [`Step`] as an index.
    pub(super) steps: [StepCounts; 3],
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.unhashable += other.unhashable;
        self.no_identity += other.no_identity;
        self.unshareable += other.unshareable;
        self.unencodable += other.unencodable;
        self.dropped += other.dropped;
        for (into, from) in self.steps.iter_mut().zip(&other.steps) {
            into.hits += from.hits;
            into.ran += from.ran;
            into.refused += from.refused;
        }
    }
}

/// Why offered steps were not kept since the last flush.
#[derive(Default, Debug)]
pub(super) struct Tallies {
    /// By the name of the read that kept them out, most first, then by name.
    pub(super) refused: Vec<(&'static str, u64)>,
    pub(super) counts: Counts,
}

/// One shard's entries, taken at a flush.
pub(super) struct Taken {
    bytes: Vec<u8>,
    defs: Vec<DefWithBodyId>,
}

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

/// What one offer buffers.
pub(super) struct Offered<'a> {
    pub(super) step: Step,
    pub(super) def: DefWithBodyId,
    pub(super) base: &'a ContentId,
    pub(super) shape: &'a [u8],
    pub(super) values: &'a [ContentId],
    pub(super) answer: &'a [u8],
}

impl Pending {
    pub(super) fn new() -> Pending {
        // alloc: setup: once per installed store
        Pending { shards: (0..SHARDS).map(|_| Padded::default()).collect() }
    }

    fn shard(&self) -> MutexGuard<'_, Shard> {
        let at = SHARD.with(|at| *at);
        self.shards[at].0.lock()
    }

    pub(super) fn push(&self, offered: Offered<'_>) {
        let Offered { step, def, base, shape, values, answer } = offered;
        let entry_len = ENTRY_HEAD + shape.len() + 32 * values.len() + answer.len();
        let mut shard = self.shard();
        if shard.bytes.len() + entry_len > SHARD_BYTES {
            shard.tallies.dropped += 1;
            return;
        }
        let len = |it: usize| u32::try_from(it).expect("an entry is shorter than a shard");
        let Shard { bytes, defs, .. } = &mut *shard;
        // alloc: the shard's buffer, at most `SHARD_BYTES`, kept from flush to flush
        bytes.reserve(entry_len);
        bytes.extend_from_slice(&len(entry_len).to_le_bytes());
        bytes.extend_from_slice(&[step as u8, 0, 0, 0]);
        bytes.extend_from_slice(&len(shape.len()).to_le_bytes());
        bytes.extend_from_slice(&len(answer.len()).to_le_bytes());
        bytes.extend_from_slice(&base.0);
        bytes.extend_from_slice(shape);
        for value in values {
            bytes.extend_from_slice(&value.0);
        }
        bytes.extend_from_slice(answer);
        // alloc: one body per entry, kept from flush to flush
        defs.push(def);
    }

    /// A lookup of `step` that found its result stored (`hit`), or ran it.
    pub(super) fn looked_up(&self, step: Step, hit: bool) {
        let mut shard = self.shard();
        let counts = &mut shard.tallies.steps[step as usize];
        match hit {
            true => counts.hits += 1,
            false => counts.ran += 1,
        }
    }

    /// A step kept out of the store, or for [`Refusal::NoIdentity`] a body not looked up.
    pub(super) fn refused(&self, step: Step, refusal: Refusal) {
        let mut shard = self.shard();
        let Shard { tallies: counts, refused, .. } = &mut *shard;
        if refusal != Refusal::NoIdentity {
            counts.steps[step as usize].refused += 1;
        }
        match refusal {
            Refusal::Read(name) => match refused.entry(name) {
                Slot::Occupied(mut count) => *count.get_mut() += 1,
                // alloc: one entry per query name, a few dozen
                Slot::Vacant(slot) => _ = slot.insert(1),
            },
            Refusal::Unhashable => counts.unhashable += 1,
            Refusal::NoIdentity => counts.no_identity += 1,
            Refusal::Unshareable => counts.unshareable += 1,
            Refusal::Unencodable => counts.unencodable += 1,
        }
    }

    /// What each step's lookups found and its offers met since the last [`Pending::take`], left
    /// in place.
    pub(super) fn steps(&self) -> [StepCounts; 3] {
        let mut counts = Counts::default();
        for shard in &self.shards {
            let shard = shard.0.lock();
            counts.add(&Counts { steps: shard.tallies.steps, ..Counts::default() });
        }
        counts.steps
    }

    /// Every shard's entries and tallies, leaving them empty.
    pub(super) fn take(&self) -> (Vec<Taken>, Tallies) {
        let mut tallies = Tallies::default();
        // alloc: flush, once per build
        let mut refused = HashMap::<&'static str, u64>::default();
        // alloc: flush, once per build
        let mut taken = Vec::with_capacity(SHARDS);
        for shard in &self.shards {
            let mut shard = shard.0.lock();
            taken.push(Taken { bytes: mem::take(&mut shard.bytes), defs: mem::take(&mut shard.defs) });
            for (name, count) in shard.refused.drain() {
                *refused.entry(name).or_default() += count;
            }
            tallies.counts.add(&mem::take(&mut shard.tallies));
        }
        // alloc: flush, once per build
        tallies.refused = refused.into_iter().collect();
        tallies.refused.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        (taken, tallies)
    }
}

/// A buffered step, read in place.
pub(super) struct Entry<'a> {
    pub(super) step: Step,
    pub(super) def: DefWithBodyId,
    pub(super) base: ContentId,
    /// The shape record, as it is stored.
    pub(super) shape: &'a [u8],
    values: &'a [u8],
    pub(super) answer: &'a [u8],
}

impl Entry<'_> {
    pub(super) fn values(&self) -> impl Iterator<Item = ContentId> + '_ {
        self.values.chunks_exact(32).map(|value| ContentId(value.try_into().expect("32 bytes")))
    }
}

/// The entries [`Pending::push`] wrote into a shard.
pub(super) fn entries(taken: &Taken) -> impl Iterator<Item = Entry<'_>> {
    let mut rest = taken.bytes.as_slice();
    let mut defs = taken.defs.iter();
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes(rest[at..at + 4].try_into().expect("4 bytes")) as usize;
        let (entry_len, shape_len, answer_len) = (word(0), word(8), word(12));
        let step = Step::ALL[usize::from(rest[4])];
        let (entry, next) = rest.split_at(entry_len);
        let base = ContentId(entry[16..48].try_into().expect("32 bytes"));
        let (shape, rest_of_entry) = entry[ENTRY_HEAD..].split_at(shape_len);
        let (values, answer) = rest_of_entry.split_at(rest_of_entry.len() - answer_len);
        rest = next;
        let def = *defs.next().expect("a body for each entry");
        Some(Entry { step, def, base, shape, values, answer })
    })
}
