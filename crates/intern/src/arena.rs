//! The store under every interner: values in append-only arenas sharded by hash.
//!
//! A value is written once into a chunk of its shard's arena and never moves or is freed: the
//! stores are statics, so a pointer to a value is good for the life of the process. Memory is
//! bounded by the database rebuilding what it keeps (docs/v2/plan/lanes/db.md), not by counting references.
//!
//! A shard's table maps a hash to the values with it, SwissTable-style ([`crate::swiss`],
//! docs/v2/plan/design/hardware.md, "interner and memo lookups"): slots in groups of seven, each group one cache line holding a
//! control word of tag bytes and the seven value pointers, so a lookup reads one line of the
//! table and then the value. Finding a value takes no lock and writes nothing; interning one
//! takes its shard's [`InsertLock`] only to look again and insert. A table grows by copying into
//! one twice its size, published once full; the tables it outgrew are kept, as a reader may be
//! probing one, so a shard's tables cost at most twice its last.
//!
//! Shards are a power of two of at least four times the core count, chosen once per process
//! (docs/v2/plan/design/concurrency.md, "Writers shard by hash"). Readers load only the packed
//! table pointers, which change when a table grows; writers lock a separate, padded array, so a
//! write never dirties a line a reader loads.
//!
//! Chunks grow to 2 MiB, claimed from [`crate::space`]'s region, which is backed by huge pages
//! where the kernel offers them (docs/v2/plan/design/hardware.md, "memory").
//!
//! Cost: a value takes one 8-byte pointer and one tag byte in its group's line (9.1 bytes a
//! slot) and an 8-byte hash in a column only growth reads, at a load of a quarter to a half: 34
//! to 68 bytes of table a value, plus the value. A found value costs one acquire load of the
//! table pointer, one of the group's control word (almost always the home group's), one pointer
//! load from the same line and the caller's `Eq`.
//!
//! Safe code: the raw memory (the groups read from zeroed pages, the published tables, the
//! chunks) is [`crate::space`]'s. Values are handed out as `NonNull<u8>`, which only
//! [`crate::typed`] reads.

use std::{alloc::Layout, ptr::NonNull, sync::OnceLock};

use self::lock::{InsertLock, Locked};
use crate::{
    space::{self, GROUP, Group, Published},
    swiss::{self, Ctrl},
    sync::atomic::{AtomicU64, Ordering},
};

const FIRST_GROUPS: usize = 2;

/// Never fewer shards than this, so a small machine still spreads its writers.
const LEAST_SHARD_BITS: u32 = 4;

const CHUNK_ALIGN: usize = 64;
const FIRST_CHUNK: usize = 1024;
/// The largest chunk, one huge page.
const LAST_CHUNK: usize = 2 * 1024 * 1024;

/// `log2` of the shard count, the same for every store of the process.
fn shard_bits() -> u32 {
    static BITS: OnceLock<u32> = OnceLock::new();
    *BITS.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .expect("intern: the core count to size interner shards (available_parallelism)")
            .get();
        (cores * 4).next_power_of_two().trailing_zeros().max(LEAST_SHARD_BITS)
    })
}

pub(crate) struct Store {
    /// Shard `i`'s table in use, null until its first insert. Read by every lookup; stored only
    /// by the holder of `writers[i]` when the table grows. Packed: a pointer changes a few
    /// dozen times in a shard's life, so sharing a line with others' costs readers nearly
    /// nothing, and eight to a line keep the array in cache.
    tables: Box<[Published<Table>]>,
    writers: Box<[InsertLock]>,
    bits: u32,
}

/// What a shard's writer owns.
struct State {
    /// The values in the shard.
    len: usize,
    bump: Bump,
}

/// Region memory in a normal build; a model build keeps boxes, as the model checker's atomics
/// are not plain words.
#[cfg(not(blaze_check))]
type Cells<T> = &'static [T];
#[cfg(blaze_check)]
type Cells<T> = Box<[T]>;

struct Table {
    groups: Cells<Group>,
    /// The hash of `groups[i / GROUP].values[i % GROUP]`, written with it and read only by
    /// the writer growing the table, so growing never rehashes a value.
    hashes: Cells<AtomicU64>,
}

impl Table {
    /// Read from fresh pages in place: a zero control word is every slot empty.
    #[cfg(not(blaze_check))]
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        Table { groups: space::zeroed(groups), hashes: space::zeroed(groups * GROUP) }
    }

    /// The checker's atomics are not plain words, so a model build makes each one.
    #[cfg(blaze_check)]
    fn new(groups: usize) -> Table {
        debug_assert!(groups.is_power_of_two());
        let group = |_| Group {
            ctrl: AtomicU64::new(0),
            values: std::array::from_fn(|_| crate::sync::atomic::AtomicPtr::new(std::ptr::null_mut())),
        };
        Table {
            // alloc: a shard's table, once a doubling, over the inserts that filled the last
            groups: (0..groups).map(group).collect(),
            // alloc: as the groups
            hashes: (0..groups * GROUP).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    fn capacity(&self) -> usize {
        self.hashes.len()
    }

    /// The value with `hash` that `matches`, probing groups from the hash's home until one with
    /// an empty slot.
    #[inline]
    fn find(&self, hash: u64, mut matches: impl FnMut(NonNull<u8>) -> bool) -> Option<NonNull<u8>> {
        let ctrl = |at: usize| Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Acquire));
        swiss::find(hash, swiss::tag(hash), self.groups.len(), ctrl, |at| {
            // Relaxed: the tag's `Acquire` above ordered the slot's writes before us; only tags
            // published in the loaded word are matched.
            let value = NonNull::new(self.groups[at.group].values[at.slot].load(Ordering::Relaxed))
                .expect("a published slot holds a value");
            matches(value).then_some(value)
        })
    }

    /// Fills the first empty slot from `hash`'s home group. Called by the holder of the shard's
    /// lock, the only writer, on a table that stays at most half full.
    fn put(&self, hash: u64, value: *mut u8, publish: Ordering) {
        let at = swiss::vacant(hash, self.groups.len(), |at: usize| {
            Ctrl::<GROUP>(self.groups[at].ctrl.load(Ordering::Relaxed))
        });
        let group = &self.groups[at.group];
        group.values[at.slot].store(value, Ordering::Relaxed);
        self.hashes[at.index()].store(hash, Ordering::Relaxed);
        group.ctrl.store(at.tagged(swiss::tag(hash)).0, publish);
    }
}

/// What [`Store::entry`] found.
pub(crate) enum Entry<'a> {
    Found(NonNull<u8>),
    /// Not there, with the shard locked until it is inserted.
    Vacant(Vacant<'a>),
}

pub(crate) struct Vacant<'a> {
    table: &'a Published<Table>,
    state: Locked<'a>,
    hash: u64,
}

impl Store {
    pub(crate) fn new() -> Store {
        let bits = shard_bits();
        let shards = 1usize << bits;
        let store = Store {
            // alloc: the shard array, once a store
            tables: (0..shards).map(|_| Published::new()).collect(),
            writers: (0..shards)
                .map(|_| InsertLock::new(State { len: 0, bump: Bump::new() }))
                // alloc: as the tables
                .collect(),
            bits,
        };
        // Where std's `Mutex` is a pthread mutex (Apple targets), its first lock boxes it:
        // taken once here, so no insert allocates.
        #[cfg(not(blaze_check))]
        for writer in &store.writers {
            // alloc: a shard lock's pthread mutex, once a store, on Apple targets
            drop(writer.lock());
        }
        store
    }

    #[inline]
    fn shard(&self, hash: u64) -> usize {
        // The probe starts from the hash's low bits and the tag is its top ones; the shard is
        // picked by all of them, mixed.
        let mixed = hash.wrapping_mul(0x9e37_79b9_7f4a_7c15);
        (mixed >> (u64::BITS - self.bits)) as usize
    }

    /// The value with `hash` that `matches`, without a lock or a write.
    #[inline]
    pub(crate) fn find(
        &self,
        hash: u64,
        matches: impl FnMut(NonNull<u8>) -> bool,
    ) -> Option<NonNull<u8>> {
        self.tables[self.shard(hash)].load(Ordering::Acquire)?.find(hash, matches)
    }

    /// The value with `hash` that `matches`, or the shard locked to insert it. `matches` is
    /// called with values of this store only, published and never freed.
    pub(crate) fn entry(
        &self,
        hash: u64,
        mut matches: impl FnMut(NonNull<u8>) -> bool,
    ) -> Entry<'_> {
        if let Some(found) = self.find(hash, &mut matches) {
            return Entry::Found(found);
        }
        let shard = self.shard(hash);
        let state = self.writers[shard].lock();
        // Another thread may have inserted it between the look above and the lock.
        if let Some(found) = self.find(hash, &mut matches) {
            return Entry::Found(found);
        }
        Entry::Vacant(Vacant { table: &self.tables[shard], state, hash })
    }

    /// How many values the store holds.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.writers.iter().map(|writer| writer.lock().len).sum()
    }

    #[cfg(test)]
    fn shards(&self) -> usize {
        self.tables.len()
    }
}

impl Vacant<'_> {
    /// Allocates `layout` in the shard's arena, has `write` initialize it and publishes it.
    pub(crate) fn insert(mut self, layout: Layout, write: impl FnOnce(NonNull<u8>)) -> NonNull<u8> {
        let at = self.state.bump.alloc(layout);
        write(at);
        self.publish(at);
        at
    }

    /// Publishes `value`, initialized by `insert` and never moved or freed, as everything
    /// [`Store::find`] hands out is.
    fn publish(mut self, value: NonNull<u8>) {
        let table = self.table_for_one_more();
        table.put(self.hash, value.as_ptr(), Ordering::Release);
        self.state.len += 1;
    }

    /// The table to insert one more value into, at most half full after it.
    fn table_for_one_more(&mut self) -> &'static Table {
        let current = self.table.load(Ordering::Acquire);
        let capacity = current.map_or(0, Table::capacity);
        if let Some(table) = current
            && (self.state.len + 1) * 2 <= capacity
        {
            return table;
        }
        let groups = current.map_or(FIRST_GROUPS, |table| table.groups.len() * 2);
        let grown = Table::new(groups);
        if let Some(old) = current {
            // Relaxed: every write to the table was made by a holder of the lock we hold.
            for (at, group) in old.groups.iter().enumerate() {
                for slot in Ctrl::<GROUP>(group.ctrl.load(Ordering::Relaxed)).full() {
                    let hash = old.hashes[at * GROUP + slot].load(Ordering::Relaxed);
                    grown.put(hash, group.values[slot].load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
        }
        // A model build's table holds boxes, so it is leaked as one.
        #[cfg(blaze_check)]
        // alloc: the model checker's build only, once a doubling
        let grown: &'static Table = Box::leak(Box::new(grown));
        // Kept for good, as readers may hold the last.
        #[cfg(not(blaze_check))]
        let grown: &'static Table = space::place(grown);
        // Release: a reader that loads the new table sees its slots and, through the lock's
        // order, the values they point to.
        self.table.store(grown, Ordering::Release);
        grown
    }
}

mod lock {
    #![allow(
        clippy::disallowed_types,
        reason = "the one lock of `intern`, named and bounded as docs/v2/plan/design/concurrency.md asks"
    )]

    use std::ops::{Deref, DerefMut};

    use super::State;
    use crate::sync::{Mutex, MutexGuard};

    /// The lock a shard's writers take to look again for a value and insert it. It is held
    /// only for that: the caller's `Eq` against the shard's values, one bump allocation, the
    /// value's write and the slot's publication. Nothing called under it reaches a query or
    /// another lock, so it can't be held across a query call; a reader never takes it.
    ///
    /// Padded to 128 bytes, the pair of lines arm64 and x86 prefetch together, so writers of
    /// two shards never share a line.
    #[repr(align(128))]
    pub(super) struct InsertLock(Mutex<State>);

    pub(super) struct Locked<'a>(MutexGuard<'a, State>);

    impl InsertLock {
        pub(super) fn new(state: State) -> InsertLock {
            InsertLock(Mutex::new(state))
        }

        pub(super) fn lock(&self) -> Locked<'_> {
            // A panic under the lock (in a caller's `Eq`) leaves the shard as it was.
            Locked(self.0.lock())
        }
    }

    impl Deref for Locked<'_> {
        type Target = State;

        fn deref(&self) -> &State {
            &self.0
        }
    }

    impl DerefMut for Locked<'_> {
        fn deref_mut(&mut self) -> &mut State {
            &mut self.0
        }
    }
}

/// A bump allocator over chunks that are never freed or reused. It keeps offsets into
/// [`crate::space`]'s region, whose base is page-aligned, so an offset is aligned exactly when its
/// address is, and the address comes from the region with its provenance: no raw pointer is
/// held, so the allocator is `Send` and safe.
struct Bump {
    /// The next free byte of the current chunk, from the region's base; 0 before the first.
    cursor: usize,
    /// The bytes left in the current chunk from `cursor`.
    left: usize,
    /// The size of the next chunk.
    next: usize,
}

impl Bump {
    fn new() -> Bump {
        Bump { cursor: 0, left: 0, next: FIRST_CHUNK }
    }

    /// `layout.size()` bytes (at least one, so every value has its own address) aligned to
    /// `layout.align()`, uninitialized, never freed.
    fn alloc(&mut self, layout: Layout) -> NonNull<u8> {
        let size = layout.size().max(1);
        let align = layout.align();
        if align > CHUNK_ALIGN || size > LAST_CHUNK / 4 {
            return space::claim(Layout::from_size_align(size, align).expect("a value's layout"));
        }
        let mut pad = self.cursor.next_multiple_of(align) - self.cursor;
        if pad + size > self.left {
            let bytes = self.next.max(size.next_multiple_of(CHUNK_ALIGN));
            self.next = (self.next * 2).min(LAST_CHUNK);
            self.cursor =
                space::claim_offset(Layout::from_size_align(bytes, CHUNK_ALIGN).expect("a chunk's layout"));
            self.left = bytes;
            pad = 0;
        }
        let at = self.cursor + pad;
        self.cursor = at + size;
        self.left -= pad + size;
        space::at(at)
    }
}

/// Under the model checker (`RUSTFLAGS="--cfg blaze_check" blaze test -p intern --release
/// model_`): racing inserts and lock-free finds, on an empty store and across a table growth,
/// against a set: an insert is new exactly when the value was not there, a find sees what was
/// inserted before it, and each value has one address and is read whole.
#[cfg(all(test, blaze_check))]
#[allow(unsafe_code, reason = "the model's values are `u64`s written and read through the store's raw pointers")]
mod model {
    use std::sync::Arc;

    use blaze_check::{
        linear::{History, Spec},
        thread,
    };

    use super::*;

    /// One hash for every value: one shard, one home group and one tag, so every probe
    /// compares values and seven values fill the first table to half.
    const HASH: u64 = 7;

    fn read(at: NonNull<u8>) -> u64 {
        // SAFETY: Every value of the model's stores is a `u64`, written before it was published.
        unsafe { *at.cast::<u64>().as_ref() }
    }

    /// `value`'s address, and whether this call inserted it.
    fn intern(store: &Store, value: u64) -> (usize, bool) {
        let (at, new) = match store.entry(HASH, |at| read(at) == value) {
            Entry::Found(at) => (at, false),
            Entry::Vacant(vacant) => {
                // SAFETY: `insert` hands `write` fresh memory of `Layout::new::<u64>()`, aligned
                // and written by no one else.
                let at = vacant.insert(Layout::new::<u64>(), |to| unsafe { to.cast::<u64>().write(value) });
                (at, true)
            }
        };
        assert_eq!(read(at), value, "an interned value is read whole");
        (at.as_ptr().addr(), new)
    }

    fn find(store: &Store, value: u64) -> Option<usize> {
        let at = store.find(HASH, |at| read(at) == value)?;
        assert_eq!(read(at), value, "a found value is read whole");
        Some(at.as_ptr().addr())
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Intern(u64),
        Find(u64),
    }

    /// The sequential store: bit `v` is set when value `v` is in it.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Set(u16);

    impl Spec for Set {
        type Op = Op;
        /// `Intern`: whether it inserted. `Find`: whether it found.
        type Ret = bool;

        fn apply(&mut self, op: &Op) -> bool {
            match *op {
                Op::Intern(value) => {
                    let new = self.0 & (1 << value) == 0;
                    self.0 |= 1 << value;
                    new
                }
                Op::Find(value) => self.0 & (1 << value) != 0,
            }
        }
    }

    /// Runs `op` as one operation of `history`: the address it interned or found.
    fn run(history: &History<Set>, store: &Store, op: Op) -> Option<usize> {
        let mut at = None;
        history.record(op, |&op| match op {
            Op::Intern(value) => {
                let (addr, new) = intern(store, value);
                at = Some(addr);
                new
            }
            Op::Find(value) => {
                at = find(store, value);
                at.is_some()
            }
        });
        at
    }

    /// First inserts into an empty store: one of the two threads makes and publishes the shard's
    /// first table, and a find racing with it sees nothing or the whole value.
    #[test]
    fn model_first_inserts_publish_a_table() {
        blaze_check::model(|| {
            let store = Arc::new(Store::new());
            let history = Arc::new(History::<Set>::new());
            let other = {
                let (store, history) = (store.clone(), history.clone());
                thread::spawn(move || {
                    let one = run(&history, &store, Op::Intern(1));
                    let two = run(&history, &store, Op::Find(2));
                    (one, two)
                })
            };
            let two = run(&history, &store, Op::Intern(2));
            let one = run(&history, &store, Op::Find(1));
            let (one_at, two_found) = other.join().unwrap();
            history.check(Set::default());
            assert_ne!(one_at, two, "two values, two addresses");
            if one.is_some() {
                assert_eq!(one, one_at, "1 found where it was interned");
            }
            if two_found.is_some() {
                assert_eq!(two_found, two, "2 found where it was interned");
            }
            assert_eq!(store.len(), 2);
        });
    }

    /// Seven values fill the first table to half, so whichever of 7 and 8 goes in first grows
    /// it, while the other thread finds a value from before the growth and races the insert.
    #[test]
    fn model_inserts_race_across_growth() {
        blaze_check::model(|| {
            let store = Arc::new(Store::new());
            let before: Vec<usize> = (0..7).map(|value| intern(&store, value).0).collect();
            let history = Arc::new(History::<Set>::new());
            let other = {
                let (store, history) = (store.clone(), history.clone());
                thread::spawn(move || [Op::Intern(7), Op::Intern(8)].map(|op| run(&history, &store, op)))
            };
            let zero = run(&history, &store, Op::Find(0));
            let eight = run(&history, &store, Op::Intern(8));
            let seven = run(&history, &store, Op::Find(7));
            let [seven_at, eight_at] = other.join().unwrap();
            history.check(Set(0x7f));
            assert_eq!(zero, Some(before[0]), "a value from before the growth is found where it was");
            assert_eq!(eight, eight_at, "8 interned once");
            if seven.is_some() {
                assert_eq!(seven, seven_at, "7 found where it was interned");
            }
            let mut all: Vec<usize> = before.iter().copied().chain(seven_at).chain(eight_at).collect();
            all.sort_unstable();
            all.dedup();
            assert_eq!(all.len(), 9, "nine values, nine addresses");
            assert_eq!(store.len(), 9);
            for (value, &at) in before.iter().enumerate() {
                assert_eq!(find(&store, value as u64), Some(at), "kept across the growth");
            }
        });
    }
}

#[cfg(all(test, not(blaze_check)))]
#[allow(unsafe_code, reason = "the tests' values are `u64`s written and read through the store's raw pointers")]
mod tests {
    use std::collections::HashMap;

    use blaze_ids::rng::SplitMix64;

    use super::*;

    fn read(at: NonNull<u8>) -> u64 {
        // SAFETY: Every value of the test stores is a `u64`.
        unsafe { *at.cast::<u64>().as_ref() }
    }

    fn intern(store: &Store, hash: u64, value: u64) -> NonNull<u8> {
        match store.entry(hash, |at| read(at) == value) {
            Entry::Found(at) => at,
            Entry::Vacant(vacant) => vacant
                // SAFETY: `insert` hands `write` fresh memory of `Layout::new::<u64>()`, aligned
                // and written by no one else.
                .insert(Layout::new::<u64>(), |to| unsafe { to.cast::<u64>().write(value) }),
        }
    }

    #[test]
    fn shards_scale_with_cores() {
        let store = Store::new();
        let cores = std::thread::available_parallelism().unwrap().get();
        assert!(store.shards().is_power_of_two());
        assert!(store.shards() >= cores * 4);
        assert!(store.shards() >= 1 << LEAST_SHARD_BITS);
        assert!(store.shards() < (cores * 8).max(2 << LEAST_SHARD_BITS));
        assert_eq!(store.writers.len(), store.shards());
        assert_eq!(align_of::<InsertLock>(), 128);
    }

    /// Values of one hash all land in one shard, one home group and one tag, and probe past
    /// each other: the table grows many times and every value is still found where it was
    /// written.
    #[test]
    fn colliding_hashes_grow_one_shard() {
        let store = Store::new();
        let first: Vec<_> = (0..5_000).map(|n| intern(&store, 7, n)).collect();
        assert_eq!(store.len(), 5_000);
        for (n, &at) in first.iter().enumerate() {
            assert_eq!(intern(&store, 7, n as u64), at);
            assert_eq!(read(at), n as u64);
        }
        assert_eq!(store.len(), 5_000);
    }

    /// Hashes that share a home group and differ in tag, and ones that share a tag and differ
    /// in the rest: each value is found under its own hash, and a value never inserted is not.
    #[test]
    fn tags_and_groups_tell_values_apart() {
        let store = Store::new();
        let hashes: Vec<u64> = (0..2_000u64)
            .map(|n| if n % 2 == 0 { (n << 57) | 3 } else { (5 << 57) | (n << 8) })
            .collect();
        let at: Vec<_> = hashes.iter().map(|&hash| intern(&store, hash, hash)).collect();
        for (&hash, &at) in hashes.iter().zip(&at) {
            assert_eq!(store.find(hash, |it| read(it) == hash), Some(at));
            let other = hash ^ (1 << 56);
            assert_eq!(store.find(other, |it| read(it) == other), None);
        }
    }

    /// The table against a `HashMap` model, over seeded hashes drawn from few enough values
    /// that many collide in tag, group and shard: the same values are found, each at the
    /// address of its first insert, and the counts agree.
    #[test]
    fn table_agrees_with_a_model() {
        for seed in 0..4 {
            let store = Store::new();
            let mut model: HashMap<u64, NonNull<u8>> = HashMap::new();
            let mut rng = SplitMix64::new(seed);
            for _ in 0..30_000 {
                let value = rng.below(9_000);
                // Few distinct hashes for many values, and some hashes that differ only in the
                // tag or only in the home group.
                let hash = match value % 3 {
                    0 => value % 97,
                    1 => (value % 61) << 57 | 5,
                    _ => value.wrapping_mul(0xff51_afd7_ed55_8ccd),
                };
                let at = intern(&store, hash, value);
                assert_eq!(read(at), value);
                assert_eq!(*model.entry(value).or_insert(at), at, "seed {seed}");
            }
            assert_eq!(store.len(), model.len());
        }
    }

    /// Many threads interning overlapping seeded values get one address per value.
    #[test]
    fn racing_writers_agree() {
        let store = Store::new();
        let found: Vec<Vec<(u64, usize)>> = std::thread::scope(|scope| {
            let workers: Vec<_> = (0..16)
                .map(|seed| {
                    let store = &store;
                    scope.spawn(move || {
                        let mut rng = SplitMix64::new(seed);
                        (0..20_000)
                            .map(|_| {
                                let value = rng.below(5_000);
                                let at = intern(store, value % 211, value);
                                assert_eq!(read(at), value);
                                (value, at.as_ptr().addr())
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers.into_iter().map(|it| it.join().unwrap()).collect()
        });
        let mut one: HashMap<u64, usize> = HashMap::new();
        for (value, addr) in found.into_iter().flatten() {
            assert_eq!(*one.entry(value).or_insert(addr), addr);
        }
        assert_eq!(store.len(), one.len());
    }

    #[test]
    fn found_without_lock_after_insert() {
        let store = Store::new();
        let at = intern(&store, 42, 1);
        assert_eq!(store.find(42, |it| read(it) == 1), Some(at));
        assert_eq!(store.find(42, |it| read(it) == 2), None);
        assert_eq!(store.find(43, |_| true), None);
    }

    #[test]
    fn bump_aligns_and_never_overlaps() {
        let mut bump = Bump::new();
        let mut taken: Vec<(usize, usize)> = Vec::new();
        for i in 0..40_000usize {
            let align = 1 << (i % 8);
            let size = (i * 7) % 300;
            let at = bump.alloc(Layout::from_size_align(size, align).unwrap());
            assert_eq!(at.as_ptr().addr() % align, 0);
            taken.push((at.as_ptr().addr(), size.max(1)));
        }
        let large = bump.alloc(Layout::from_size_align(1 << 20, 8).unwrap());
        taken.push((large.as_ptr().addr(), 1 << 20));
        let over_aligned = bump.alloc(Layout::from_size_align(8, 4096).unwrap());
        assert_eq!(over_aligned.as_ptr().addr() % 4096, 0);
        taken.push((over_aligned.as_ptr().addr(), 8));
        taken.sort_unstable();
        for pair in taken.windows(2) {
            assert!(pair[0].0 + pair[0].1 <= pair[1].0, "{pair:?}");
        }
        assert_eq!(bump.next, LAST_CHUNK, "the test reached huge chunks");
    }
}
