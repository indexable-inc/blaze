//! The append-only tables [`Files`](crate::Files) keeps beside the snapshot, read without a
//! lock (docs/v2/plan/design/concurrency.md): a read is acquire loads of data that never moves
//! once published, and only writers write. The snapshot's own rows (each file's text, each
//! root, each crate) are vectors of the snapshot's table ([`crate::world::Table`]), made at a
//! commit.
//!
//! - [`Log`]: an append-only list whose values never move, a `blaze_mem::Column`.
//! - [`Table`]: values found by a hash the caller computed once, in a `blaze_mem::swiss::Index`
//!   of `u32`s per shard (as `blaze_ids::Sharded`), grown by copying into a table twice the
//!   size. A lookup is acquire loads and compares; inserting locks one shard to claim a place,
//!   never while making the value.

use std::{alloc::Global, sync::OnceLock};

use blaze_core::lock::Lock;
use blaze_hash::SipKey;
use blaze_mem::Tag;
use intern::swiss::Index;

/// The largest file ID the front end takes (`span::EditionedFileId` holds 23 bits of one).
pub(crate) const MAX_FILE_ID: u32 = span::EditionedFileId::MAX_FILE_ID;

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

/// Values found by a 64-bit hash of their key ([`Table::hash`], computed once by the caller),
/// each kept once. The hash is SipHash-1-3 under the table's own key, drawn at random: the keys
/// are `include!` paths and `env!` names a crate's source picks, so a hash anyone can compute
/// would let it pick thousands with one hash, each insert then probing past all of them.
///
/// 6.7 to 13.3 bytes of index a value, plus the value and its hash in a [`Log`]. A lookup is
/// one shard pick, a control word and a slot and a key compare per entry whose hash
/// matches; nothing is written.
pub(crate) struct Table<T> {
    /// What [`Table::hash`] is keyed by.
    key: SipKey,
    values: Log<(u64, T)>,
    /// Made on its first insert: a table costs 16 bytes a shard until then.
    shards: Box<[OnceLock<Box<Shard>>]>,
    /// A hash's shard is its top bits.
    shift: u32,
}

struct Shard {
    /// Held to insert; holds how many values the shard has.
    lock: Lock<u32>,
    /// The shard's values by [`probe_hash`] of their hash.
    index: Index,
}

impl<T> Table<T> {
    /// At most `max` values.
    pub(crate) fn new(max: usize) -> Table<T> {
        let shards = blaze_ids::shard_count();
        Table {
            key: SipKey::random(),
            values: Log::new(max),
            // Once per table: 16 bytes a shard.
            shards: {
                let mut all = Vec::with_capacity_in(shards, Global);
                all.extend((0..shards).map(|_| OnceLock::new()));
                all.into_boxed_slice()
            },
            shift: u64::BITS - shards.trailing_zeros(),
        }
    }

    /// The hash `key` is found by in this table, the one [`Table::find`] and [`Table::insert`]
    /// are given.
    #[inline]
    pub(crate) fn hash(&self, key: impl std::hash::Hash) -> u64 {
        self.key.hash_one(&key)
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
            // A shard, on its first insert.
            Box::new_in(Shard { lock: Lock::new("db.table.shard", 0), index: Index::new() }, Global)
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

    /// The value at `raw`, `None` for an index this table has not handed out.
    #[inline]
    pub(crate) fn try_get(&self, raw: u32) -> Option<&T> {
        Some(&self.values.get(raw)?.1)
    }

    /// Every value, in the order they were inserted (which threads decide: sort what leaves).
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> + '_ {
        self.values.iter().map(|(_, value)| value)
    }
}

/// What a shard's [`Index`] is keyed by: the shard is the hash's top bits, so the index's tag
/// (its top seven) and start (its low ones) come from the hash folded and mixed.
fn probe_hash(hash: u64) -> u64 {
    (hash ^ (hash >> 32)).wrapping_mul(0x9e37_79b9_7f4a_7c15)
}
