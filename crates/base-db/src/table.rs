//! The tables [`Files`](crate::Files) reads on every query, without a lock
//! (docs/v2/plan/design/concurrency.md): a read is acquire loads of data that never moves once
//! published, and only writers write.
//!
//! - [`Column`]: a query input per dense `u32` ID (a file, a source root), in a
//!   `blaze_mem::AppendVec` reserved for every ID. A read is two acquire loads.
//! - [`Log`]: an append-only list whose values never move, a `blaze_mem::Column`.
//! - [`Table`]: values found by a hash the caller computed once, in a `blaze_mem::swiss::Index`
//!   of `u32`s per shard (as `blaze_ids::Sharded`), grown by copying into a table twice the
//!   size. A lookup is acquire loads and compares; inserting locks one shard to claim a place,
//!   never while making the value.

use std::{
    marker::PhantomData,
    sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    },
};

use blaze_mem::{AppendVec, Tag};
use blaze_query::db::{AsId, FromId, Id};
use intern::swiss::Index;
use parking_lot::Mutex;

/// A query input per ID up to [`Column::MAX`]: a word of the input's ID per ID, 0 for none
/// (no ID is 0), in one region reserved for `MAX + 1` words and committed as
/// IDs are used.
///
/// A read is one acquire load of the length, a bounds compare and the word's acquire load, and
/// writes nothing.
pub(crate) struct Column<T> {
    words: AppendVec<AtomicU32>,
    of: PhantomData<fn() -> T>,
}

impl<T: AsId + FromId + Copy> Column<T> {
    /// The largest file ID the front end takes (`span::EditionedFileId` holds 23 bits of one):
    /// a column indexes `0..=MAX`.
    pub(crate) const MAX: u32 = span::EditionedFileId::MAX_FILE_ID;

    pub(crate) fn new() -> Column<T> {
        let words = AppendVec::new(Self::MAX as usize + 1, Tag::Query)
            .unwrap_or_else(|error| panic!("{error}"));
        Column { words, of: PhantomData }
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> Option<T> {
        match self.words.get(raw)?.load(Ordering::Acquire) {
            0 => None,
            bits => Some(T::from_id(Id::from_u32(bits))),
        }
    }

    /// Sets (or with `None` clears) the input of `raw`. Called only by the setters, with the
    /// files' writer lock held: the one writer `AppendVec` asks for. Panics naming it past
    /// [`Column::MAX`].
    pub(crate) fn set(&self, raw: u32, value: Option<T>) {
        assert!(
            raw <= Self::MAX,
            "ID {raw} is past {}, the largest file ID the front end takes",
            Self::MAX
        );
        let bits = value.map_or(0, |it| it.as_id().as_u32());
        if bits == 0 && raw >= self.words.len() {
            return;
        }
        let len = self.words.len();
        if len <= raw {
            self.words.writer().extend_with((raw - len + 1) as usize, |_| AtomicU32::new(0));
        }
        self.words.as_slice()[raw as usize].store(bits, Ordering::Release);
    }

    /// Every ID that has an input, in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (u32, T)> + '_ {
        self.words.iter().enumerate().filter_map(|(raw, word)| match word.load(Ordering::Acquire) {
            0 => None,
            bits => Some((raw as u32, T::from_id(Id::from_u32(bits)))),
        })
    }
}

/// An append-only list whose values never move: `&T` lives as long as the log. Appending
/// claims an index with one `fetch_add`; a read is an acquire load and a compare.
pub(crate) struct Log<T>(blaze_mem::Column<T>);

impl<T> Log<T> {
    /// At most `max` values.
    pub(crate) fn new(max: usize) -> Log<T> {
        Log(blaze_mem::Column::new(max, Tag::Query).unwrap_or_else(|error| panic!("{error}")))
    }

    pub(crate) fn push(&self, value: T) -> u32 {
        self.0.push(value).unwrap_or_else(|error| panic!("{error}"))
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> Option<&T> {
        self.0.get(raw)
    }

    /// The values appended, in order; one still being appended is left out.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.0.iter().map(|(_, value)| value)
    }
}

/// Values found by a 64-bit hash of their key (computed once by the caller), each kept once.
///
/// 6.7 to 13.3 bytes of index a value, plus the value and its hash in a [`Log`]. A lookup is
/// one shard pick, a control word and a slot and a key compare per entry whose hash
/// matches; nothing is written.
pub(crate) struct Table<T> {
    values: Log<(u64, T)>,
    /// Made on its first insert: a table costs 16 bytes a shard until then.
    shards: Box<[OnceLock<Box<Shard>>]>,
    /// A hash's shard is its top bits.
    shift: u32,
}

struct Shard {
    /// Held to insert; holds how many values the shard has.
    lock: Mutex<u32>,
    /// The shard's values by [`probe_hash`] of their hash.
    index: Index,
}

impl<T> Table<T> {
    /// At most `max` values.
    pub(crate) fn new(max: usize) -> Table<T> {
        let shards = blaze_ids::shard_count();
        Table {
            values: Log::new(max),
            // alloc: once per table: 16 bytes a shard.
            shards: (0..shards).map(|_| OnceLock::new()).collect(),
            shift: u64::BITS - shards.trailing_zeros(),
        }
    }

    fn shard(&self, hash: u64) -> &OnceLock<Box<Shard>> {
        &self.shards[(hash >> self.shift) as usize]
    }

    fn probe(&self, shard: &Shard, hash: u64, eq: &impl Fn(&T) -> bool) -> Option<u32> {
        shard.index.find(probe_hash(hash), |raw| {
            let (stored, value) = self.values.get(raw).expect("a table entry before its value");
            *stored == hash && eq(value)
        })
    }

    /// The index of the value whose key `eq` matches, `hash` being its key's hash.
    #[inline]
    pub(crate) fn find(&self, hash: u64, eq: impl Fn(&T) -> bool) -> Option<u32> {
        self.probe(self.shard(hash).get()?, hash, &eq)
    }

    /// The index of the value whose key `eq` matches, inserting `make()` if there is none, and
    /// whether it did. `make` runs with the shard locked: it must not insert into this table,
    /// call into the database or block.
    pub(crate) fn insert(
        &self,
        hash: u64,
        eq: impl Fn(&T) -> bool,
        make: impl FnOnce() -> T,
    ) -> (u32, bool) {
        if let Some(found) = self.find(hash, &eq) {
            return (found, false);
        }
        let shard = self.shard(hash).get_or_init(|| {
            // alloc: a shard, on its first insert.
            Box::new(Shard { lock: Mutex::new(0), index: Index::new() })
        });
        let mut count = shard.lock.lock();
        if let Some(found) = self.probe(shard, hash, &eq) {
            return (found, false);
        }
        let raw = self.values.push((hash, make()));
        let hash_of =
            |raw: u32| probe_hash(self.values.get(raw).expect("a table entry before its value").0);
        shard.index.insert(*count as usize, probe_hash(hash), raw, hash_of);
        *count += 1;
        (raw, true)
    }

    #[inline]
    pub(crate) fn get(&self, raw: u32) -> &T {
        &self.values.get(raw).expect("an index this table did not hand out").1
    }

    /// Every value, in the order they were inserted (which threads decide: sort what leaves).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.values.iter().map(|(_, value)| value)
    }
}

/// The hash a [`Table`] is keyed by: `blaze_hash::FixedState`, as every key is the build's own (paths and names
/// its sources wrote), never an attacker's.
#[inline]
pub(crate) fn hash_of(key: impl std::hash::Hash) -> u64 {
    use std::hash::BuildHasher;
    blaze_hash::FixedState.hash_one(key)
}

/// What a shard's [`Index`] is keyed by: the shard is the hash's top bits, so the index's tag
/// (its top seven) and start (its low ones) come from the hash folded and mixed.
fn probe_hash(hash: u64) -> u64 {
    (hash ^ (hash >> 32)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
