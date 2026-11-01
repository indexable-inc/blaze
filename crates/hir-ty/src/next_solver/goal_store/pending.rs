//! Offered goals, buffered until [`super::Installed::flush`]: one buffer per shard of threads,
//! each its own lock and cache line, so an offer takes an uncontended lock and appends, and no
//! worker waits on the store.
//!
//! An entry is its length, its answer, its shape's length, its shape (`record`'s bytes, as
//! stored) and its facts' values, each 32 bytes, in the shape's order.

use std::{
    collections::hash_map::Entry as Slot,
    mem,
    sync::atomic::{AtomicUsize, Ordering},
};

use blaze_hash::HashMap;
use blaze_ids::ContentId;
use parking_lot::Mutex;

use super::{Answer, FactRef, Refusal, record};

const SHARDS: usize = 64;

/// The most a shard buffers between flushes: past it a goal is dropped and counted
/// ([`super::Flushed::dropped`]), so a build that offers more than a flush writes keeps its
/// memory bounded. 64 MiB in all.
const SHARD_BYTES: usize = 1 << 20;

/// Entry length, answer, shape length.
const ENTRY_HEAD: usize = 4 + 1 + 4;

pub(super) struct Pending {
    shards: Box<[Padded]>,
}

#[repr(align(128))]
#[derive(Default)]
struct Padded(Mutex<Shard>);

#[derive(Default)]
struct Shard {
    bytes: Vec<u8>,
    inexpressible: HashMap<&'static str, u64>,
    unhashable: u64,
    too_long: u64,
    dropped: u64,
}

/// Why offered goals were not kept since the last flush.
#[derive(Default, Debug)]
pub(super) struct Tallies {
    /// Most first, then by name.
    pub(super) inexpressible: Vec<(&'static str, u64)>,
    pub(super) unhashable: u64,
    pub(super) too_long: u64,
    pub(super) dropped: u64,
}

static NEXT_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static SHARD: usize = NEXT_SHARD.fetch_add(1, Ordering::Relaxed) % SHARDS;
}

impl Pending {
    pub(super) fn new() -> Pending {
        // alloc: setup: once per installed store
        Pending { shards: (0..SHARDS).map(|_| Padded::default()).collect() }
    }

    fn shard(&self) -> parking_lot::MutexGuard<'_, Shard> {
        let at = SHARD.with(|at| *at);
        self.shards[at].0.lock()
    }

    pub(super) fn push(
        &self,
        goal_key: &ContentId,
        version: &ContentId,
        answer: Answer,
        words: &[u32],
        facts: &[FactRef],
        values: &[ContentId],
    ) {
        debug_assert_eq!(facts.len(), values.len());
        let shape_len = record::shape_len(words.len(), facts.len());
        let entry_len = ENTRY_HEAD + shape_len + 32 * values.len();
        let mut shard = self.shard();
        if shard.bytes.len() + entry_len > SHARD_BYTES {
            shard.dropped += 1;
            return;
        }
        let bytes = &mut shard.bytes;
        // alloc: the shard's buffer, at most `SHARD_BYTES`, kept from flush to flush
        bytes.reserve(entry_len);
        bytes.extend_from_slice(&(entry_len as u32).to_le_bytes());
        bytes.push(match answer {
            Answer::Fails => 0,
            Answer::Holds => 1,
        });
        bytes.extend_from_slice(&(shape_len as u32).to_le_bytes());
        let start = bytes.len();
        record::write_shape(bytes, goal_key, version, words, facts);
        debug_assert_eq!(bytes.len() - start, shape_len);
        for value in values {
            bytes.extend_from_slice(&value.0);
        }
    }

    pub(super) fn refused(&self, refusal: Refusal) {
        let mut shard = self.shard();
        match refusal {
            Refusal::Read(name) => match shard.inexpressible.entry(name) {
                Slot::Occupied(mut count) => *count.get_mut() += 1,
                // alloc: one entry per query name, a few dozen
                Slot::Vacant(slot) => _ = slot.insert(1),
            },
            Refusal::Unhashable => shard.unhashable += 1,
            Refusal::TooLong => shard.too_long += 1,
        }
    }

    /// Every shard's entries and tallies, leaving them empty.
    pub(super) fn take(&self) -> (Vec<Vec<u8>>, Tallies) {
        let mut tallies = Tallies::default();
        // alloc: flush, once per build
        let mut inexpressible = HashMap::<&'static str, u64>::default();
        // alloc: flush, once per build
        let mut buffers = Vec::with_capacity(SHARDS);
        for shard in &self.shards {
            let mut shard = shard.0.lock();
            buffers.push(mem::take(&mut shard.bytes));
            for (name, count) in shard.inexpressible.drain() {
                *inexpressible.entry(name).or_default() += count;
            }
            tallies.unhashable += mem::take(&mut shard.unhashable);
            tallies.too_long += mem::take(&mut shard.too_long);
            tallies.dropped += mem::take(&mut shard.dropped);
        }
        // alloc: flush, once per build
        tallies.inexpressible = inexpressible.into_iter().collect();
        tallies.inexpressible.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        (buffers, tallies)
    }
}

/// A buffered goal, read in place.
pub(super) struct Entry<'a> {
    pub(super) goal_key: ContentId,
    pub(super) answer: Answer,
    /// The shape record, as it is stored.
    pub(super) shape: &'a [u8],
    values: &'a [u8],
}

impl Entry<'_> {
    pub(super) fn values(&self) -> impl Iterator<Item = ContentId> + '_ {
        self.values.chunks_exact(32).map(|value| ContentId(value.try_into().expect("32 bytes")))
    }
}

/// The entries [`Pending::push`] wrote into `bytes`.
pub(super) fn entries(bytes: &[u8]) -> impl Iterator<Item = Entry<'_>> {
    let mut rest = bytes;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes(rest[at..at + 4].try_into().expect("4 bytes")) as usize;
        let entry_len = word(0);
        let answer = match rest[4] {
            0 => Answer::Fails,
            _ => Answer::Holds,
        };
        let shape_len = word(5);
        let (entry, next) = rest.split_at(entry_len);
        let (shape, values) = entry[ENTRY_HEAD..].split_at(shape_len);
        rest = next;
        Some(Entry {
            goal_key: ContentId(shape[8..40].try_into().expect("a shape starts with its magic and goal key")),
            answer,
            shape,
            values,
        })
    })
}
