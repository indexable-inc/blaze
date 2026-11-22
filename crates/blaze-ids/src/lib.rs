//! blaze v2's IDs (docs/v2/plan/design/data.md section 5, "IDs, two kinds").
//!
//! - A [`ContentId`] is the BLAKE3 hash of what a thing is, under a domain that names the
//!   kind of thing and its schema. It is the identity used in every hash, key and output.
//! - An [`Idx`] is a dense `u32` index into one arena, assigned in arrival order by parallel
//!   workers: it differs between runs, so it never goes into a hash, a key or the bytes of an
//!   output. It may be kept in the daemon's image, which is one run's state.
//! - An [`Interner`] holds one arena and maps each way between the two; [`Sharded`] is the
//!   one blaze uses.
//! - A [`Fingerprint`] is the 128 bits of a result kept for early cutoff (docs/v2/plan/design/item-hash.md
//!   section 6.2).

// `min_specialization`: implementing `blaze_hash::PlainKey`, a specialization trait, needs it.
#![feature(allocator_api, allocator_ext, min_specialization, portable_simd, register_tool)]
#![register_tool(blaze)]
#![deny(unsafe_code)]

use std::{alloc::Global, fmt, hash, marker::PhantomData, num::NonZeroUsize};

// The once-cell, the spin lock and the atomics are blaze-swiss's: `core`'s atomics, or the
// model checker's in a model build (docs/v2/plan/design/model-checker.md, "The switch").
use blaze_swiss::{
    Index,
    sync::{
        Backoff, Once, Spin,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
};

use crate::b3::ContextKey;

pub mod b3;
pub mod bits;
#[allow(unsafe_code)]
pub mod crc32c;
/// [`ContentId::of_array`] and [`ContentId::of_each`]: many IDs into the caller's slots.
mod each;
pub mod fuse;
pub mod keyed;
mod many;
pub mod names;
pub mod producer;
pub use blaze_radix as radix;
pub use blaze_rng as rng;
mod std_hash;
// blaze-data's forms of the IDs (`ToValue`, `FromValue`): one byte string each.
mod data;
// serde's forms of the IDs, for the crates above that persist them with serde: behind the
// `serde` feature, which they turn on (the root manifest's entry), so this crate depends on no
// crate from outside the workspace (docs/v2/plan/design/bottom.md, "Also decided on 2026-11-15").
#[cfg(feature = "serde")]
mod wire;

pub use blaze_hash::fixed_width_writes;

/// Counts allocations so the read paths' tests hold them to a budget.
#[cfg(all(test, not(blaze_check)))]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The BLAKE3 hash of what something is. Two things with one content ID are the same thing.
/// Serialized as one byte string of 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct ContentId(pub [u8; 32]);

impl ContentId {
    /// The content ID of `bytes` as a thing of `domain`. A domain names what the bytes are
    /// and the version of their schema (`"blaze v2 file 1"`): equal bytes of two domains have
    /// two IDs.
    pub fn of(domain: &'static str, bytes: &[u8]) -> ContentId {
        hashed(bytes.len(), 1);
        ContentId(b3::derive_key_with(context_key(domain), bytes))
    }

    /// [`ContentId::of`] for a [`Domain`], its context key read with one acquire load.
    pub fn of_domain(domain: &Domain, bytes: &[u8]) -> ContentId {
        hashed(bytes.len(), 1);
        ContentId(b3::derive_key_with(domain.key(), bytes))
    }

    /// `inputs.iter().map(|it| ContentId::of(domain, it))`, many inputs a SIMD lane each.
    ///
    /// An input of at most one BLAKE3 chunk (1 KiB) is hashed as one chunk: its blocks
    /// before the last are compressed beside the other inputs with as many (16 at once on
    /// AVX-512, 8 elsewhere), and its last block, whatever its length, beside other inputs'
    /// last blocks. A longer input goes through the hasher, which hashes its whole chunks
    /// side by side.
    pub fn of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId> {
        hashed(inputs.iter().map(|it| it.len()).sum(), inputs.len());
        many::of_many(context_key(domain), inputs)
    }

    /// [`ContentId::of_many`] into `out` (cleared first): a warm call with room allocates
    /// nothing.
    pub fn of_many_into(domain: &'static str, inputs: &[&[u8]], out: &mut Vec<ContentId>) {
        hashed(inputs.iter().map(|it| it.len()).sum(), inputs.len());
        many::of_many_into::<b3::Many>(context_key(domain), inputs, out)
    }

    /// The first 128 bits, as a result's fingerprint.
    pub fn fingerprint(&self) -> Fingerprint {
        let mut low = [0; 16];
        low.copy_from_slice(&self.0[..16]);
        Fingerprint(u128::from_le_bytes(low))
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for ContentId {
    /// The first 12 hex digits, as git shows a short id.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0[..6].iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

impl fmt::LowerHex for ContentId {
    /// All 64 hex digits.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|byte| write!(f, "{byte:02x}"))
    }
}

/// Builds a [`ContentId`] from parts. It takes bytes and other content IDs only, so a dense
/// [`Idx`] can't be hashed but by spelling out [`Idx::raw`], which a reviewer sees.
pub struct Hasher(b3::Hasher);

/// The [`blaze_hash::SipKey`] an image's index of `values` (in the order the image keeps them)
/// is laid out by: the first 16 bytes of their BLAKE3 in `domain`, each value's length before
/// it. A function of the values alone, so the same values give the same image (AGENTS.md rule
/// 22); and unknown until they are all chosen, so none can be chosen to collide under it. Every
/// persisted index of keys an attacker may pick (the interner's, the VFS's names) takes its
/// key here, each in a domain of its own.
pub fn index_key<'a>(
    domain: &'static str,
    values: impl IntoIterator<Item = &'a [u8]>,
) -> blaze_hash::SipKey {
    let mut hasher = Hasher::new(domain);
    for value in values {
        hasher.update(&(value.len() as u64).to_le_bytes()).update(value);
    }
    let id = hasher.finish();
    let word = |at: usize| {
        u64::from_le_bytes(id.0[at..at + 8].try_into().expect("8 bytes of a content ID"))
    };
    blaze_hash::SipKey::new(word(0), word(8))
}

/// A hash domain whose BLAKE3 context key is derived once, on first use: what a domain hashed
/// millions of times is declared as (`pub static KEY: Domain = Domain::new("blaze v2 key 1")`).
pub struct Domain {
    name: &'static str,
    key: Once<ContextKey>,
}

impl Domain {
    pub const fn new(name: &'static str) -> Domain {
        Domain { name, key: Once::new() }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    fn key(&self) -> &ContextKey {
        self.key.get_or_init(|| b3::context_key(self.name))
    }

    /// The domain's BLAKE3 context key, derived on first use: what a batch of inputs is
    /// hashed under side by side (`blaze_blake3::derive_key_many`), each one's digest the
    /// [`ContentId::of_domain`] of it.
    pub fn context_key(&self) -> &[u8; 32] {
        self.key()
    }
}

/// How many domain names [`Hasher::new`] keeps the context keys of: about 500 literals name a
/// domain in the workspace, and one name at two addresses takes two slots.
const DOMAIN_SLOTS: usize = 2048;

/// The context key of each domain name hashed so far, open-addressed by the name's address:
/// a slot once set never changes, so a lookup is acquire loads and compares.
static DOMAIN_KEYS: DomainKeys = [const { Once::new() }; DOMAIN_SLOTS];

type DomainKeys = [Once<(&'static str, ContextKey)>; DOMAIN_SLOTS];

/// A domain's first slot, from its address and length, so a lookup reads none of its text: a
/// domain hashed millions of times is one literal, found by a pointer compare. Hashing the text
/// (FNV-1a, a dependent multiply a byte) cost about 70 cycles per [`ContentId::of`], a third of
/// a one-block hash. A name at a second address gets a slot of its own holding the same key, as
/// the key is derived from the text.
fn home(name: &str) -> usize {
    let word = name.as_ptr().addr() as u64 ^ (name.len() as u64).rotate_right(16);
    (word.wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 40) as usize & (DOMAIN_SLOTS - 1)
}

/// [`b3::context_key`]`(domain)`, derived once per name.
#[blaze::pure(
    trusted = "DOMAIN_KEYS memoizes b3::context_key of the domain's text: a slot once set never changes, and a lookup compares the text, so the key is the text's whatever the table holds"
)]
fn context_key(domain: &'static str) -> &'static ContextKey {
    context_key_in(&DOMAIN_KEYS, domain)
}

/// [`context_key`] in the table `slots`, which a model test makes fresh for each execution.
fn context_key_in<'a>(slots: &'a DomainKeys, domain: &'static str) -> &'a ContextKey {
    let mut at = home(domain);
    for _ in 0..DOMAIN_SLOTS {
        let slot = &slots[at];
        let (name, key) = match slot.get() {
            Some(entry) => entry,
            None => slot.get_or_init(|| (domain, b3::context_key(domain))),
        };
        if std::ptr::eq(*name, domain) || *name == domain {
            return key;
        }
        at = (at + 1) & (DOMAIN_SLOTS - 1);
    }
    panic!("more than {DOMAIN_SLOTS} hash domains: raise blaze-ids' DOMAIN_SLOTS");
}

impl Hasher {
    pub fn new(domain: &'static str) -> Hasher {
        Hasher(b3::Hasher::new_from_context_key(context_key(domain)))
    }

    pub fn of(domain: &Domain) -> Hasher {
        Hasher(b3::Hasher::new_from_context_key(domain.key()))
    }

    pub fn update(&mut self, bytes: &[u8]) -> &mut Hasher {
        hashed(bytes.len(), 0);
        self.0.update(bytes);
        self
    }

    /// A length-prefixed part, so `("ab", "c")` and `("a", "bc")` hash apart.
    pub fn part(&mut self, bytes: &[u8]) -> &mut Hasher {
        hashed(8 + bytes.len(), 0);
        let len = (bytes.len() as u64).to_le_bytes();
        // A part that fits a block with its length goes in as one update, the length and the
        // bytes together: one call and one append to the hasher's held block, not two. The
        // bytes fed are the same, so the digest is.
        if bytes.len() <= 56 {
            let mut framed = [0u8; 64];
            framed[..8].copy_from_slice(&len);
            framed[8..8 + bytes.len()].copy_from_slice(bytes);
            self.0.update(&framed[..8 + bytes.len()]);
        } else {
            self.0.update(&len);
            self.0.update(bytes);
        }
        self
    }

    /// An optional part: `0` for none, else `1` and the [`part`](Hasher::part).
    pub fn opt_part(&mut self, part: Option<impl AsRef<[u8]>>) -> &mut Hasher {
        match part {
            None => self.update(&[0]),
            Some(part) => self.update(&[1]).part(part.as_ref()),
        }
    }

    pub fn id(&mut self, id: &ContentId) -> &mut Hasher {
        hashed(32, 0);
        self.0.update(&id.0);
        self
    }

    pub fn finish(&self) -> ContentId {
        hashed(0, 1);
        ContentId(self.0.finalize())
    }
}

/// The thread's meter (D169): `bytes` fed to BLAKE3 and `digests` made. Two thread-local adds.
#[inline(always)]
fn hashed(bytes: usize, digests: usize) {
    blaze_meter::add(blaze_meter::Count::Hashed, bytes as u64);
    blaze_meter::add(blaze_meter::Count::Digests, digests as u64);
}

/// 128 bits that stand for a result's value: a rerun that produces the same fingerprint stops
/// there (early cutoff). Kept with every dependency edge and when the value is evicted.
/// Serialized as one byte string of its 16 little-endian bytes: CBOR has no integer
/// this wide.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(transparent)]
pub struct Fingerprint(pub u128);

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}", (self.0 >> 96) as u32)
    }
}

/// A dense index into the arena of `T`s that made it. Only meaningful for that arena.
#[repr(transparent)]
pub struct Idx<T> {
    raw: u32,
    of: PhantomData<fn() -> T>,
}

impl<T> Idx<T> {
    pub const fn from_raw(raw: u32) -> Idx<T> {
        Idx { raw, of: PhantomData }
    }

    pub const fn raw(self) -> u32 {
        self.raw
    }
}

impl<T> Clone for Idx<T> {
    fn clone(&self) -> Idx<T> {
        *self
    }
}

impl<T> Copy for Idx<T> {}

impl<T> PartialEq for Idx<T> {
    fn eq(&self, other: &Idx<T>) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Idx<T> {}

impl<T> PartialOrd for Idx<T> {
    fn partial_cmp(&self, other: &Idx<T>) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<T> Ord for Idx<T> {
    fn cmp(&self, other: &Idx<T>) -> std::cmp::Ordering {
        self.raw.cmp(&other.raw)
    }
}

impl<T> hash::Hash for Idx<T> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.raw.hash(state);
    }
}

// SAFETY: `#[repr(transparent)]` over a `u32` and a zero-sized `PhantomData`, so no padding
// and no address; equal exactly when `raw` is, so when the four bytes are. Not uniform: an
// index is small and sequential, so `hash_plain` mixes it.
#[allow(unsafe_code, reason = "a `PlainKey` impl: the promise its SAFETY comment states")]
unsafe impl<T> blaze_hash::PlainKey for Idx<T> {}

// SAFETY: `#[repr(transparent)]` over `[u8; 32]` with derived `Eq`: no padding, no address,
// equal exactly when the bytes are. Uniform: a BLAKE3 digest, so its first word is its hash,
// as `ByHash` takes it.
#[allow(unsafe_code, reason = "a `PlainKey` impl: the promise its SAFETY comment states")]
unsafe impl blaze_hash::PlainKey for ContentId {
    const UNIFORM: bool = true;
}

// SAFETY: `#[repr(transparent)]` over a `u128` with derived `Eq`: no padding, no address,
// equal exactly when the bytes are. Uniform: its bits are a BLAKE3 digest's first 128
// (`ContentId::fingerprint`).
#[allow(unsafe_code, reason = "a `PlainKey` impl: the promise its SAFETY comment states")]
unsafe impl blaze_hash::PlainKey for Fingerprint {
    const UNIFORM: bool = true;
}

impl<T> fmt::Debug for Idx<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{}", self.raw)
    }
}

/// One arena of `T`s, append-only and sharded by content ID: interning from many workers
/// takes no global lock, and a reader never locks. A value once interned never moves or
/// changes, so `&T` lives as long as the interner.
pub trait Interner<T>: Sync {
    /// The index of the value whose content ID is `id`, interning `value` if it has none.
    /// `value` must be what `id` is the content ID of. It may run and its value be dropped
    /// when another worker interns `id` at the same time.
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T>;
    fn get(&self, idx: Idx<T>) -> &T;
    fn content(&self, idx: Idx<T>) -> ContentId;
    fn find(&self, id: &ContentId) -> Option<Idx<T>>;
    fn len(&self) -> usize;
}

/// The [`Interner`] of blaze v2: values in an append-only arena of chunks that never move,
/// found by content ID in one of its [`Shards`]' [`blaze_swiss::Index`]es.
///
/// [`Interner::get`], [`Interner::content`] and [`Interner::find`] take no lock. Interning a
/// new value computes it first, then locks the one shard its content ID falls in only to look
/// again and insert (a racing loser's value is dropped): O(1) under the lock. A shard's index
/// grows into one twice its size, built by the writer that filled it to three quarters after
/// it unlocks, while others go on inserting, and swapped in under the lock with what they
/// inserted meanwhile ([`blaze_swiss::Grow`]); the tables it outgrew are kept until the
/// interner is dropped, as a reader may be probing one, so the tables cost at most twice the
/// last one.
///
/// Not `intern`'s interners: those key a value by its own bytes and live for the process;
/// this one is keyed by a content ID its caller computed (so it hashes nothing), owns values
/// of any type and drops them with itself, and gives dense indices of one run.
///
/// A shard hands out indices from a block of [`BLOCK`] it claims at a time, so writers to
/// different shards share no counter. Indices are dense but for the unused rest of each
/// shard's last block: every index is below [`Sharded::end`], at most `BLOCK - 1` per shard
/// past [`Interner::len`].
pub struct Sharded<T> {
    slots: Chunks<T>,
    /// The first index no shard has claimed.
    next: AtomicU64,
    shards: Box<[Shard]>,
    /// `shards.len() - 1`.
    mask: usize,
}

/// How many indices a shard claims at a time.
pub const BLOCK: u32 = 64;

/// How many shards a [`Sharded`] (or another sharded table) has: a power of two, made from the
/// worker count its maker passes in, so nothing here asks the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shards(u32);

impl Shards {
    /// Shards per worker: four times as many shards as writers, so two writers rarely want
    /// one shard at once, as in `blaze-hashcons`. Not measured yet: change 8's bench in
    /// docs/v2/plan/design/parallel.md decides it.
    pub const PER_WORKER: usize = 4;

    /// `workers` (the scheduler's worker count) rounded up to a power of two, times
    /// [`Shards::PER_WORKER`]: at least 4.
    pub const fn for_workers(workers: NonZeroUsize) -> Shards {
        let count = workers.get().next_power_of_two() * Self::PER_WORKER;
        assert!(count <= u32::MAX as usize, "a shard count below 2^32");
        Shards(count as u32)
    }

    pub fn count(self) -> usize {
        self.0 as usize
    }

    /// For a table made once per process (a `static` interner), which has no scheduler to ask:
    /// [`Shards::for_workers`] of the machine's available parallelism, read once, here, by
    /// name. A table with a maker that knows its worker count passes it instead.
    pub fn for_this_machine() -> Shards {
        let workers = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Shards::for_workers(workers)
    }
}

/// Chunk `k` holds `FIRST << k` slots, so an index's chunk and place in it are arithmetic and
/// 27 chunks hold every `u32`.
const FIRST: usize = 64;
const CHUNKS: usize = 27;

type Chunks<T> = [Once<Box<[Once<(ContentId, T)>]>>; CHUNKS];

/// The indices `next..end` a shard has claimed and not yet handed out.
struct Claimed {
    next: u32,
    end: u32,
}

/// A value on cache lines of its own: 128 bytes on Apple silicon, whose lines are 128 bytes,
/// and 64 elsewhere.
#[cfg_attr(all(target_arch = "aarch64", target_vendor = "apple"), repr(C, align(128)))]
#[cfg_attr(not(all(target_arch = "aarch64", target_vendor = "apple")), repr(C, align(64)))]
struct Line<T>(T);

/// Lines of its own per shard, so writers to neighbouring shards don't share a line, and the
/// lock on a line apart from the index's level, which every find reads.
#[cfg_attr(all(target_arch = "aarch64", target_vendor = "apple"), repr(C, align(128)))]
#[cfg_attr(not(all(target_arch = "aarch64", target_vendor = "apple")), repr(C, align(64)))]
struct Shard {
    /// The one lock of `Sharded`, held only to probe again, insert into the index and claim a
    /// block of indices: O(1), never the copy of a growth, never across a call
    /// (docs/v2/plan/design/concurrency.md), and a find never takes it. A spin lock, as
    /// nothing below this crate parks a thread.
    lock: Line<Spin<Claimed>>,
    /// How many values the shard has, written under `lock`.
    len: AtomicU32,
    /// The shard's values by [`probe_hash`]: a find is one control word and one slot, then
    /// the slot's ID compared only on a tag match. On the global heap, as the interner's
    /// chunks are.
    index: Index<Global>,
}

/// Bits 64..128 of the ID: its probe's start (the low bits) and tag (the top seven). The shard
/// was picked by bits 0..32, so these are independent of it, and an ID is already a hash.
fn probe_hash(id: &ContentId) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&id.0[8..16]);
    u64::from_le_bytes(word)
}

fn chunk_of(raw: u32) -> (usize, usize) {
    let biased = raw as usize / FIRST + 1;
    let chunk = (usize::BITS - 1 - biased.leading_zeros()) as usize;
    (chunk, raw as usize - FIRST * ((1 << chunk) - 1))
}

impl<T: Send + Sync> Sharded<T> {
    pub fn new(shards: Shards) -> Sharded<T> {
        let count = shards.count();
        Sharded {
            slots: std::array::from_fn(|_| Once::new()),
            next: AtomicU64::new(0),
            shards: (0..count)
                .map(|_| Shard {
                    lock: Line(Spin::new("ids.sharded.claim", Claimed { next: 0, end: 0 })),
                    len: AtomicU32::new(0),
                    index: Index::new_in(Global),
                })
                // alloc: once per interner made, its shards
                .collect(),
            mask: count - 1,
        }
    }

    /// Above every index handed out so far, by at most [`BLOCK`] per shard: what a column
    /// indexed by [`Idx`] is sized to.
    pub fn end(&self) -> usize {
        self.next.load(Ordering::Acquire) as usize
    }

    fn slot(&self, raw: u32) -> Option<&(ContentId, T)> {
        let (chunk, at) = chunk_of(raw);
        self.slots[chunk].get()?.get(at)?.get()
    }

    fn shard(&self, id: &ContentId) -> &Shard {
        let mut word = [0; 4];
        word.copy_from_slice(&id.0[..4]);
        &self.shards[u32::from_le_bytes(word) as usize & self.mask]
    }

    /// The index of `id` in `shard`, taking no lock.
    fn probe(&self, shard: &Shard, id: &ContentId) -> Option<Idx<T>> {
        let is = |raw: u32| self.slot(raw).expect("an index entry before its value").0 == *id;
        shard.index.find(probe_hash(id), is).map(Idx::from_raw)
    }
}

impl<T: Send + Sync> Interner<T> for Sharded<T> {
    fn intern(&self, id: ContentId, value: impl FnOnce() -> T) -> Idx<T> {
        if let Some(found) = self.find(&id) {
            return found;
        }
        let value = value();
        let shard = self.shard(&id);
        let hash_of =
            |raw: u32| probe_hash(&self.slot(raw).expect("an index entry before its value").0);
        // The wait for a growth another writer is building, once the table is seven eighths
        // full: rare, as the growth starts at three quarters.
        let mut grown = Backoff::new("ids.sharded.growth");
        loop {
            let mut claimed = shard.lock.0.lock();
            if let Some(found) = self.probe(shard, &id) {
                return found;
            }
            let len = shard.len.load(Ordering::Relaxed);
            let vacancy = match shard.index.vacancy(len as usize) {
                Ok(vacancy) => vacancy,
                Err(full) => {
                    drop(claimed);
                    match full.grow {
                        // The writer that held it panicked: this one builds it instead.
                        Some(grow) => {
                            let built = grow.build(hash_of);
                            let _claimed = shard.lock.0.lock();
                            built.publish(hash_of);
                        }
                        None => {
                            while shard.index.level() <= full.level {
                                grown.snooze();
                            }
                        }
                    }
                    continue;
                }
            };
            if claimed.next == claimed.end {
                let start = self.next.fetch_add(BLOCK as u64, Ordering::AcqRel);
                assert!(start + BLOCK as u64 <= u32::MAX as u64, "an interner of 2^32 values");
                *claimed = Claimed { next: start as u32, end: start as u32 + BLOCK };
            }
            let raw = claimed.next;
            claimed.next += 1;
            let (chunk, at) = chunk_of(raw);
            let slots = self.slots[chunk]
                // alloc: once per chunk of slots, each twice the last: amortized, never per intern
                .get_or_init(|| (0..FIRST << chunk).map(|_| Once::new()).collect());
            if slots[at].set((id, value)).is_err() {
                unreachable!("index {raw} handed out twice");
            }
            let grow = vacancy.fill(probe_hash(&id), raw);
            shard.len.store(len + 1, Ordering::Relaxed);
            drop(claimed);
            // The O(n) copy, with the lock free: others insert into the old table meanwhile
            // and the publish copies what they logged.
            if let Some(grow) = grow {
                let built = grow.build(hash_of);
                let _claimed = shard.lock.0.lock();
                built.publish(hash_of);
            }
            return Idx::from_raw(raw);
        }
    }

    fn get(&self, idx: Idx<T>) -> &T {
        &self.slot(idx.raw()).expect("an index this interner did not hand out").1
    }

    fn content(&self, idx: Idx<T>) -> ContentId {
        self.slot(idx.raw()).expect("an index this interner did not hand out").0
    }

    /// No lock, no write and no allocation: a shard with no table yet holds nothing.
    fn find(&self, id: &ContentId) -> Option<Idx<T>> {
        self.probe(self.shard(id), id)
    }

    fn len(&self) -> usize {
        self.shards.iter().map(|shard| shard.len.load(Ordering::Relaxed) as usize).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Four shards, as for one worker: few, so tests meet on shards and grow them.
    const SHARDS: Shards = Shards::for_workers(NonZeroUsize::MIN);

    /// The tag-then-part each caller (base-db's `input.rs` and `lib.rs`, hir-def's `deps`,
    /// blaze-items' `key.rs`) wrote by hand before [`Hasher::opt_part`].
    #[test]
    fn an_optional_part_is_a_tag_then_the_part() {
        for part in [None, Some(""), Some("1.2.3"), Some("a name of some length")] {
            let mut by_hand = Hasher::new("blaze v2 a 1");
            match part {
                None => by_hand.update(&[0]),
                Some(part) => by_hand.update(&[1]).part(part.as_bytes()),
            };
            let mut shared = Hasher::new("blaze v2 a 1");
            shared.opt_part(part);
            assert_eq!(shared.finish(), by_hand.finish(), "{part:?}");
            let mut owned = Hasher::new("blaze v2 a 1");
            owned.opt_part(part.map(str::to_owned));
            assert_eq!(owned.finish(), by_hand.finish(), "{part:?}, owned");
        }
        let (mut none, mut empty) = (Hasher::new("blaze v2 a 1"), Hasher::new("blaze v2 a 1"));
        none.opt_part(None::<&str>);
        empty.opt_part(Some(""));
        assert_ne!(none.finish(), empty.finish());
    }

    #[test]
    fn domains_part_equal_bytes() {
        assert_ne!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 b 1", b"x"));
        assert_eq!(ContentId::of("blaze v2 a 1", b"x"), ContentId::of("blaze v2 a 1", b"x"));
    }

    const DOMAINS: [&str; 5] = [
        "blaze v2 a 1",
        "blaze v2 b 1",
        "",
        "blaze v2 key 1",
        "blaze v2 a domain of some length 7",
    ];

    #[test]
    fn a_hasher_derives_its_domain_key() {
        for domain in DOMAINS {
            for bytes in [&b""[..], b"x", &[7; 1025]] {
                let mut expected = blake3::Hasher::new_derive_key(domain);
                expected.update(bytes);
                let expected = ContentId(*expected.finalize().as_bytes());
                assert_eq!(ContentId::of(domain, bytes), expected, "{domain:?}");
                assert_eq!(ContentId::of(domain, bytes), expected, "{domain:?}, again");
                let named = Domain::new(domain);
                assert_eq!(named.name(), domain);
                assert_eq!(ContentId::of_domain(&named, bytes), expected, "{domain:?}");
            }
        }
    }

    #[test]
    fn of_many_is_of_for_each() {
        let lengths = [0, 1, 63, 64, 65, 1024, 1025, 3000, 2, 64];
        let inputs: Vec<Vec<u8>> = lengths
            .iter()
            .enumerate()
            .map(|(i, &len)| (0..len).map(|b| (b * 7 + i) as u8).collect())
            .collect();
        let inputs: Vec<&[u8]> = inputs.iter().map(Vec::as_slice).collect();
        for domain in DOMAINS {
            let expected: Vec<ContentId> =
                inputs.iter().map(|it| ContentId::of(domain, it)).collect();
            assert_eq!(ContentId::of_many(domain, &inputs), expected, "{domain:?}");
        }
        assert_eq!(ContentId::of_many("blaze v2 a 1", &[]), Vec::new());
    }

    /// `of_many_into` is `of_many`, and a warm one into a buffer with room allocates nothing.
    #[cfg(not(blaze_check))]
    #[test]
    fn a_warm_of_many_into_allocates_nothing() {
        let bytes: Vec<u8> = (0..2_000u32).map(|n| n as u8).collect();
        // Under Miri, enough inputs for every lane count `of_many` batches by.
        let count = if cfg!(miri) { 40 } else { 600 };
        let inputs: Vec<&[u8]> = (0..count).map(|n| &bytes[n % 7..n % 7 + n % 1_100]).collect();
        let expected = ContentId::of_many("blaze v2 a 1", &inputs);
        let mut out = Vec::new();
        ContentId::of_many_into("blaze v2 a 1", &inputs, &mut out);
        assert_eq!(out, expected);
        blaze_alloc_count::assert_at_most(0, "a warm of_many_into", || {
            ContentId::of_many_into("blaze v2 a 1", &inputs, &mut out);
        });
        assert_eq!(out, expected);
    }

    /// A warm [`Hasher::new`] finds its domain's key by address and allocates nothing, and
    /// neither do short updates nor the finish.
    #[cfg(not(blaze_check))]
    #[test]
    fn a_warm_hasher_allocates_nothing() {
        let hash = || {
            let mut hasher = Hasher::new("blaze v2 a 1");
            for n in 0..40u8 {
                hasher.update(&[n; 3]).part(b"ab");
            }
            hasher.finish()
        };
        let expected = hash();
        assert_eq!(blaze_alloc_count::assert_at_most(0, "a warm Hasher", hash), expected);
    }

    #[test]
    fn of_many_is_the_hasher_at_every_length_and_group_size() {
        let bytes: Vec<u8> =
            (0..2100u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        for copies in [1, 2, 3, 4, 5, 8, 16, 17, 33] {
            let inputs: Vec<&[u8]> = (0..=1100)
                .flat_map(|len| (0..copies).map(move |copy| (len, copy)))
                .map(|(len, copy)| &bytes[copy * 31..copy * 31 + len])
                .collect();
            let many = ContentId::of_many("blaze v2 a 1", &inputs);
            for (input, id) in inputs.iter().zip(&many) {
                let mut expected = blake3::Hasher::new_derive_key("blaze v2 a 1");
                expected.update(input);
                assert_eq!(
                    *id,
                    ContentId(*expected.finalize().as_bytes()),
                    "{} bytes",
                    input.len()
                );
            }
        }
    }

    #[test]
    fn ids_of_one_tag_are_told_apart_by_their_value() {
        let interner = Sharded::<u32>::new(SHARDS);
        let mut ids = Vec::new();
        for n in 0..200u32 {
            let mut bytes = [0; 32];
            bytes[..4].copy_from_slice(&7u32.to_le_bytes());
            bytes[8..12].copy_from_slice(&0xdead_beefu32.to_le_bytes());
            bytes[16..20].copy_from_slice(&n.to_le_bytes());
            ids.push(ContentId(bytes));
        }
        let indices: Vec<_> =
            ids.iter().zip(0..).map(|(&id, n)| interner.intern(id, || n)).collect();
        for (n, (id, &idx)) in ids.iter().zip(&indices).enumerate() {
            assert_eq!(interner.find(id), Some(idx));
            assert_eq!(*interner.get(idx), n as u32);
        }
        assert_eq!(interner.len(), 200);
    }

    #[test]
    fn a_find_in_an_empty_shard_makes_no_table() {
        let interner = Sharded::<u8>::new(SHARDS);
        assert_eq!(interner.find(&id(3)), None);
        assert!(interner.shards.iter().all(|shard| shard.index.capacity() == 0));
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn many_domains_keep_their_own_keys() {
        let names: Vec<&'static str> = (0..600)
            .map(|n| &*Box::leak(format!("blaze v2 test domain {n}").into_boxed_str()))
            .collect();
        let first: Vec<ContentId> = names.iter().map(|&name| ContentId::of(name, b"x")).collect();
        let threads: Vec<Vec<ContentId>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        names.iter().map(|&name| ContentId::of(name, b"x")).collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        for (name, id) in names.iter().zip(&first) {
            let mut expected = blake3::Hasher::new_derive_key(name);
            expected.update(b"x");
            assert_eq!(*id, ContentId(*expected.finalize().as_bytes()), "{name}");
        }
        for ids in threads {
            assert_eq!(ids, first);
        }
        let copy: &'static str = Box::leak(names[3].to_owned().into_boxed_str());
        assert_eq!(ContentId::of(copy, b"x"), first[3], "the same name at another address");
    }

    #[test]
    fn shards_are_a_power_of_two_of_four_per_worker() {
        for workers in [1, 2, 3, 8, 12, 128] {
            let count = Shards::for_workers(NonZeroUsize::new(workers).unwrap()).count();
            assert!(count.is_power_of_two() && count >= 4 * workers, "{workers}: {count}");
            assert!(count < 8 * workers, "{workers}: {count}");
        }
        assert_eq!(Sharded::<u8>::new(SHARDS).shards.len(), SHARDS.count());
    }

    /// Every shard on lines of its own, and the lock on a line apart from the index.
    #[cfg(not(blaze_check))]
    #[test]
    fn a_shard_keeps_its_lock_on_its_own_line() {
        let line = if cfg!(all(target_arch = "aarch64", target_vendor = "apple")) { 128 } else { 64 };
        assert_eq!(align_of::<Shard>(), line);
        assert_eq!(size_of::<Shard>() % line, 0);
        assert_eq!(size_of::<Line<Spin<Claimed>>>(), line);
        assert_eq!(std::mem::offset_of!(Shard, len), line);
    }

    /// Real threads, 8 and as many as the machine has, interning overlapping keys into few
    /// shards, so shards grow while others claim in them: every key gets exactly one index,
    /// exactly one value per key is kept and every other one made is dropped, and `len` counts
    /// the keys.
    #[cfg(not(blaze_check))]
    #[test]
    fn threads_claim_each_key_exactly_once() {
        use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

        /// A value that counts its drops.
        struct Made<'a> {
            key: u64,
            dropped: &'a AtomicUsize,
        }

        impl Drop for Made<'_> {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Relaxed);
            }
        }

        const KEYS: u64 = if cfg!(miri) { 300 } else { 30_000 };
        let cores = std::thread::available_parallelism().map_or(1, |it| it.get());
        let counts = if cfg!(miri) { [2, 3] } else { [8, cores] };
        for threads in counts {
            let (made, dropped) = (AtomicUsize::new(0), AtomicUsize::new(0));
            let interner = Sharded::<Made<'_>>::new(SHARDS);
            let seen: Vec<Vec<(u64, Idx<Made<'_>>)>> = std::thread::scope(|scope| {
                let workers: Vec<_> = (0..threads as u64)
                    .map(|worker| {
                        let (interner, made, dropped) = (&interner, &made, &dropped);
                        scope.spawn(move || {
                            (0..KEYS)
                                .map(|n| (n * 7 + worker * 4_099) % KEYS)
                                .map(|key| {
                                    let idx = interner.intern(id(key), || {
                                        made.fetch_add(1, Relaxed);
                                        Made { key, dropped }
                                    });
                                    (key, idx)
                                })
                                .collect()
                        })
                    })
                    .collect();
                workers.into_iter().map(|it| it.join().unwrap()).collect()
            });
            assert_eq!(interner.len(), KEYS as usize, "{threads} threads: keys claimed");
            let mut raws: Vec<u32> =
                (0..KEYS).map(|key| interner.find(&id(key)).unwrap().raw()).collect();
            raws.sort_unstable();
            raws.dedup();
            assert_eq!(raws.len(), KEYS as usize, "{threads} threads: one index per key");
            for (key, idx) in seen.iter().flatten() {
                assert_eq!(interner.find(&id(*key)), Some(*idx), "{threads} threads: key {key}");
                assert_eq!(interner.get(*idx).key, *key);
            }
            let (made, lost) = (made.load(Relaxed), dropped.load(Relaxed));
            assert_eq!(made - lost, KEYS as usize, "{threads} threads: one value kept per key");
            drop(interner);
            assert_eq!(dropped.load(Relaxed), made, "{threads} threads: every value dropped");
        }
    }

    #[test]
    fn a_value_may_intern_into_its_own_interner() {
        let interner = Sharded::<u64>::new(SHARDS);
        let outer = interner.intern(id(1), || *interner.get(interner.intern(id(2), || 2)) + 1);
        assert_eq!(*interner.get(outer), 3);
        assert_eq!(interner.find(&id(2)).map(|it| *interner.get(it)), Some(2));
        assert_eq!(interner.len(), 2);
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn indices_are_dense_but_for_each_shards_last_block() {
        let interner = Sharded::<u64>::new(SHARDS);
        for n in 0..30_000 {
            interner.intern(id(n), || n);
        }
        let end = interner.end();
        let shards = interner.shards.len();
        assert!(end >= interner.len() && end - interner.len() <= shards * BLOCK as usize, "{end}");
        for n in 0..30_000 {
            assert!((interner.find(&id(n)).unwrap().raw() as usize) < end);
        }
    }

    #[test]
    fn parts_are_framed() {
        let ab_c = Hasher::new("blaze v2 test 1").part(b"ab").part(b"c").finish();
        let a_bc = Hasher::new("blaze v2 test 1").part(b"a").part(b"bc").finish();
        assert_ne!(ab_c, a_bc);
    }

    /// The folded `part` (its length and a short part in one update) feeds the bytes the two
    /// updates did, at every length across the fold's edge and after held bytes of every count.
    #[test]
    fn a_folded_part_is_its_two_updates() {
        let bytes: Vec<u8> = (0..16 * 1024 + 200u32).map(|at| (at.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        // Held counts at block edges, and at 1, 2, 4, 8 and 16 chunks and a byte either side,
        // so the part lands on a lazily merged stack and straddles a block and a chunk edge.
        let mut helds = vec![0, 1, 7, 55, 56, 63, 64, 1000, 1023];
        for chunks in [1, 2, 3, 4, 8, 16] {
            helds.extend([chunks * 1024 - 60, chunks * 1024 - 1, chunks * 1024, chunks * 1024 + 1]);
        }
        let domain = "blaze v2 test 1";
        for held in helds {
            for len in 0..=130 {
                let part = &bytes[held..held + len];
                let folded = Hasher::new(domain).update(&bytes[..held]).part(part).finish();
                let mut two = Hasher::new(domain);
                two.update(&bytes[..held]).update(&(len as u64).to_le_bytes()).update(part);
                assert_eq!(folded, two.finish(), "{held} held, a part of {len}");
                // And the bytes fed, as official BLAKE3 hashes them in one piece.
                let mut fed = bytes[..held].to_vec();
                fed.extend_from_slice(&(len as u64).to_le_bytes());
                fed.extend_from_slice(part);
                assert_eq!(folded, ContentId(blake3::derive_key(domain, &fed)), "{held} held, a part of {len}, official");
            }
        }
    }

    fn id(n: u64) -> ContentId {
        ContentId::of("blaze v2 test 1", &n.to_le_bytes())
    }

    #[test]
    fn chunks_cover_every_index() {
        let mut expected = (0, 0);
        for raw in 0..100_000u32 {
            assert_eq!(chunk_of(raw), expected, "{raw}");
            expected.1 += 1;
            if expected.1 == FIRST << expected.0 {
                expected = (expected.0 + 1, 0);
            }
        }
        assert!(chunk_of(u32::MAX).0 < CHUNKS);
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn an_interner_maps_both_ways() {
        // Under Miri, enough for each shard's index to grow four times and three slot chunks.
        const COUNT: u64 = if cfg!(miri) { 300 } else { 20_000 };
        let interner = Sharded::<u64>::new(SHARDS);
        let indices: Vec<_> = (0..COUNT).map(|n| interner.intern(id(n), || n)).collect();
        assert_eq!(interner.len(), COUNT as usize);
        for (n, &idx) in indices.iter().enumerate() {
            let n = n as u64;
            assert_eq!(*interner.get(idx), n);
            assert_eq!(interner.content(idx), id(n));
            assert_eq!(interner.find(&id(n)), Some(idx));
            assert_eq!(interner.intern(id(n), || unreachable!("interned twice")), idx);
        }
        assert_eq!(interner.find(&id(1 << 40)), None);
        assert_eq!(interner.len(), COUNT as usize);
    }

    #[cfg(not(blaze_check))]
    #[test]
    fn values_never_move() {
        let interner = Sharded::<String>::new(SHARDS);
        let first = interner.intern(id(0), || "zero".to_owned());
        let at = interner.get(first) as *const String;
        let count = if cfg!(miri) { 400 } else { 50_000 };
        for n in 1..count {
            interner.intern(id(n), || n.to_string());
        }
        assert!(std::ptr::eq(at, interner.get(first)));
    }

    #[cfg(not(blaze_check))]
    #[test]
    #[cfg_attr(miri, ignore = "threads_claim_each_key_exactly_once is its Miri-sized form")]
    fn threads_intern_one_index_per_value() {
        let interner = Sharded::<u64>::new(SHARDS);
        let seen: Vec<Vec<Idx<u64>>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..8u64)
                .map(|worker| {
                    let interner = &interner;
                    scope.spawn(move || {
                        (0..10_000u64)
                            .map(|n| (n + worker * 997) % 10_000)
                            .map(|n| interner.intern(id(n), || n))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        assert_eq!(interner.len(), 10_000);
        let mut raws: Vec<u32> =
            (0..10_000).map(|n| interner.find(&id(n)).unwrap().raw()).collect();
        raws.sort_unstable();
        raws.dedup();
        assert_eq!(raws.len(), 10_000);
        for (worker, indices) in seen.iter().enumerate() {
            for (i, &idx) in indices.iter().enumerate() {
                let n = (i as u64 + worker as u64 * 997) % 10_000;
                assert_eq!(*interner.get(idx), n);
                assert_eq!(interner.find(&id(n)), Some(idx));
            }
        }
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-ids --release
/// model_`): racing interns and lock-free finds on a [`Sharded`] interner (in one shard with
/// colliding tags, in two shards claiming blocks, across a table growth) against a set: an
/// intern keeps its value exactly when the ID was not there, a find sees every intern that
/// returned before it, an index reads back its value and ID and is below `end`, and every value
/// made is dropped once. And a [`Domain`]'s key derived by two threads at once.
#[cfg(all(test, blaze_check))]
mod model {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering::Relaxed},
    };

    use blaze_check::{
        linear::{History, Spec},
        thread,
    };

    use super::*;

    /// Four shards: keys of shards 0 and 1 fall in two.
    const SHARDS: Shards = Shards::for_workers(NonZeroUsize::MIN);

    /// An ID in shard `shard` (bits 0..32) whose probe starts from `tag` (bits 64..96; bits
    /// 96..128 are 0, so every key has one SwissTable tag), told apart by `key`.
    fn id(shard: u32, tag: u32, key: u8) -> ContentId {
        let mut bytes = [0; 32];
        bytes[..4].copy_from_slice(&shard.to_le_bytes());
        bytes[8..12].copy_from_slice(&tag.to_le_bytes());
        bytes[31] = key;
        ContentId(bytes)
    }

    /// A value that says when it is dropped. One the interner keeps is dropped only with the
    /// interner, so a value dropped by the time `intern` returns lost the race.
    struct Counted {
        key: u8,
        dropped: Arc<AtomicBool>,
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            assert!(!self.dropped.swap(true, Relaxed), "a value dropped twice");
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Intern(u8),
        Find(u8),
    }

    /// The sequential interner: bit `k` is set when key `k` is in it.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Set(u16);

    impl Spec for Set {
        type Op = Op;
        /// `Intern`: whether its value is the one kept. `Find`: whether it found the key.
        type Ret = bool;

        fn apply(&mut self, op: &Op) -> bool {
            match *op {
                Op::Intern(key) => {
                    let new = self.0 & (1 << key) == 0;
                    self.0 |= 1 << key;
                    new
                }
                Op::Find(key) => self.0 & (1 << key) != 0,
            }
        }
    }

    /// One execution's interner, the ID of each key, and every value it made.
    struct Model {
        interner: Sharded<Counted>,
        ids: Vec<ContentId>,
        made: Spin<Vec<Arc<AtomicBool>>>,
        history: History<Set>,
    }

    impl Model {
        /// Key `k` is `id(shard, tag, k)` for the `k`th of `keys`.
        fn new(keys: impl IntoIterator<Item = (u32, u32)>) -> Arc<Model> {
            let ids =
                keys.into_iter().zip(0..).map(|((shard, tag), key)| id(shard, tag, key)).collect();
            Arc::new(Model {
                interner: Sharded::new(SHARDS),
                ids,
                made: Spin::new("ids.test.made", Vec::new()),
                history: History::new(),
            })
        }

        /// Runs `op` as one operation of the history: the index it interned or found.
        fn run(&self, op: Op) -> Option<Idx<Counted>> {
            let mut at = None;
            self.history.record(op, |&op| match op {
                Op::Intern(key) => {
                    let mut made = None;
                    let idx = self.interner.intern(self.ids[key as usize], || {
                        let dropped = Arc::new(AtomicBool::new(false));
                        made = Some(dropped.clone());
                        Counted { key, dropped }
                    });
                    self.reads_back(key, idx);
                    at = Some(idx);
                    let Some(made) = made else { return false };
                    let kept = !made.load(Relaxed);
                    self.made.lock().push(made);
                    kept
                }
                Op::Find(key) => {
                    at = self.interner.find(&self.ids[key as usize]);
                    if let Some(idx) = at {
                        self.reads_back(key, idx);
                    }
                    at.is_some()
                }
            });
            at
        }

        fn reads_back(&self, key: u8, idx: Idx<Counted>) {
            assert_eq!(self.interner.get(idx).key, key, "an index reads back its value");
            assert_eq!(
                self.interner.content(idx),
                self.ids[key as usize],
                "an index reads back its ID"
            );
            assert!((idx.raw() as usize) < self.interner.end(), "an index is below `end`");
        }

        /// Checks the history, then drops the interner: every value made is dropped once.
        fn finish(model: Arc<Model>) {
            model.history.check(Set::default());
            let Ok(model) = Arc::try_unwrap(model) else {
                panic!("a model thread outlived its join")
            };
            let made = model.made.into_inner();
            drop(model.interner);
            assert!(made.iter().all(|it| it.load(Relaxed)), "every value made is dropped once");
        }
    }

    /// Two keys of one shard and one tag: a probe for either compares the other's ID, read from
    /// a slot only its published entry orders, and both take indices from the shard's one block.
    #[test]
    fn model_one_shard_colliding_tags() {
        blaze_check::model(|| {
            let model = Model::new([(0, 5), (0, 5)]);
            let other = {
                let model = model.clone();
                thread::spawn(move || [Op::Intern(0), Op::Intern(1)].map(|op| model.run(op)))
            };
            let one = model.run(Op::Intern(1));
            let zero = model.run(Op::Find(0));
            let [zero_at, one_at] = other.join().unwrap();
            assert_eq!(one, one_at, "one index for key 1");
            if zero.is_some() {
                assert_eq!(zero, zero_at, "key 0 found at its index");
            }
            assert_ne!(zero_at, one_at, "two keys, two indices");
            assert!([zero_at, one_at].iter().flatten().all(|idx| idx.raw() < BLOCK), "one block");
            assert_eq!(model.interner.len(), 2);
            Model::finish(model);
        });
    }

    /// Keys of two shards: each shard claims its own block from the shared counter, and their
    /// indices fall in two chunks, each made by whichever thread needs it first.
    #[test]
    fn model_two_shards_claim_their_own_blocks() {
        blaze_check::model(|| {
            let model = Model::new([(0, 1), (1, 1)]);
            let other = {
                let model = model.clone();
                thread::spawn(move || [Op::Intern(0), Op::Find(1)].map(|op| model.run(op)))
            };
            let one = model.run(Op::Intern(1)).expect("an intern's index");
            let zero = model.run(Op::Find(0));
            let [zero_at, one_found] = other.join().unwrap();
            let zero_at = zero_at.expect("an intern's index");
            if let Some(zero) = zero {
                assert_eq!(zero, zero_at, "key 0 found at its index");
            }
            if let Some(found) = one_found {
                assert_eq!(found, one, "key 1 found at its index");
            }
            assert_ne!(zero_at.raw() / BLOCK, one.raw() / BLOCK, "a block per shard");
            assert_eq!(model.interner.end(), 2 * BLOCK as usize);
            assert_eq!(model.interner.len(), 2);
            Model::finish(model);
        });
    }

    /// Nine keys fill a shard's second index table (two groups of six) to three quarters, so
    /// whichever of 9 and 10 goes in first starts its growth and builds the next table after
    /// unlocking, while the other thread finds a key from before the growth. The other key,
    /// the eleventh value, would pass seven eighths of the old table: its writer meets `Full`,
    /// unlocks and waits for the swap, then inserts into the new table. (Inserts logged during
    /// a build are blaze-swiss's `model_swiss_swap_keeps_an_insert_during_the_build`.)
    #[test]
    fn model_growth_keeps_every_index() {
        blaze_check::model(|| {
            let model = Model::new((0..11).map(|tag| (0, tag)));
            let before: Vec<Option<Idx<Counted>>> =
                (0..9).map(|key| model.run(Op::Intern(key))).collect();
            let other = {
                let model = model.clone();
                thread::spawn(move || [Op::Intern(9), Op::Intern(10)].map(|op| model.run(op)))
            };
            let zero = model.run(Op::Find(0));
            let ten = model.run(Op::Intern(10));
            let nine = model.run(Op::Find(9));
            let [nine_at, ten_at] = other.join().unwrap();
            assert_eq!(zero, before[0], "a key from before the growth is found at its index");
            assert_eq!(ten, ten_at, "one index for key 10");
            if nine.is_some() {
                assert_eq!(nine, nine_at, "key 9 found at its index");
            }
            let all: Vec<Option<Idx<Counted>>> =
                before.iter().copied().chain([nine_at, ten_at]).collect();
            for (key, &idx) in all.iter().enumerate() {
                assert_eq!(
                    model.interner.find(&model.ids[key]),
                    idx,
                    "key {key} kept across the growth"
                );
            }
            assert_eq!(model.interner.len(), 11);
            Model::finish(model);
        });
    }

    /// A domain's key, which every hash of it uses: the same however its first two uses race.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Derived(ContentId);

    impl Spec for Derived {
        type Op = ();
        type Ret = ContentId;

        fn apply(&mut self, (): &()) -> ContentId {
            self.0
        }
    }

    #[test]
    fn model_a_domain_key_is_derived_once() {
        const NAME: &str = "blaze v2 model 1";
        blaze_check::model(|| {
            let mut expected = blake3::Hasher::new_derive_key(NAME);
            expected.update(b"x");
            let expected = ContentId(*expected.finalize().as_bytes());
            let domain = Arc::new(Domain::new(NAME));
            let history = Arc::new(History::<Derived>::new());
            let other = {
                let (domain, history) = (domain.clone(), history.clone());
                thread::spawn(move || history.record((), |()| ContentId::of_domain(&domain, b"x")))
            };
            history.record((), |()| ContentId::of_domain(&domain, b"x"));
            other.join().unwrap();
            history.check(Derived(expected));
        });
    }

    /// Two domains whose names probe from one slot, derived at once: one sets the slot, the
    /// other reads it, sees another name and sets the next, and each gets its own key.
    #[test]
    fn model_colliding_domains_keep_their_own_keys() {
        let first = "blaze v2 model a 1";
        let first_home = home(first);
        let second: &'static str = (0..)
            .map(|n| &*Box::leak(format!("blaze v2 model b {n}").into_boxed_str()))
            .find(|name| home(name) == first_home)
            .expect("a name of the same home slot");
        blaze_check::model(move || {
            let slots: Arc<DomainKeys> = Arc::new([const { Once::new() }; DOMAIN_SLOTS]);
            let other = {
                let slots = slots.clone();
                thread::spawn(move || *context_key_in(&slots, second))
            };
            let one = *context_key_in(&slots, first);
            let two = other.join().unwrap();
            assert_eq!(one, blake3::hazmat::hash_derive_key_context(first));
            assert_eq!(two, blake3::hazmat::hash_derive_key_context(second));
            assert_eq!(*context_key_in(&slots, first), one, "found again in its slot");
            assert_eq!(*context_key_in(&slots, second), two, "found again in its slot");
        });
    }
}
