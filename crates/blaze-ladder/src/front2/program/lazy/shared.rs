//! Command-scoped shared immutable/sharded front-end cache for parallel [`super::LazyProgram`]
//! workers (Phase 3: 128-core scaling).
//!
//! Each parallel export worker runs its own small, resettable [`super::LazyProgram`] epoch over a
//! bounded bump [`blaze_mem::Frame`] while sharing ONE [`SharedLazyFront`] across all workers and
//! all worker epochs:
//!
//! 1. **Frozen heads (`BZFZ`)**: canonical position-independent head records keyed by
//!    `want_bytes(cell)` (same canonical form as `front2/tables/head.rs`), so no head is parsed or
//!    lowered more than once across 128 workers or after worker frame resets.
//! 2. **Evaluated const-generic / array-length bit patterns (`const_bits`)**: `LibKey -> u128`,
//!    eliminating redundant `eval_len` evaluations across workers.
//! 3. **Pure evaluated constants (`pure_consts`) and statics (`pure_statics`)**: self-contained
//!    [`DataRecord`] values whose relocations reference only `DataTarget::Local` or
//!    `DataTarget::Static` (no evaluator-local `InstanceKey` or `VtableKey`), eliminating 12x–128x
//!    re-evaluation of giant lookup/Unicode/PHF `const` tables (`Consts::check_as`).
//! 4. **Per-crate member & lang-member indexes (`crate_members`, `lang_members`)**: built once per
//!    `CrateItemKey` via `MemberWant::NameOnly` and shared via `Arc<SharedMemberIndex>`.
//! 5. **On-the-fly checked dependency/toolchain `Typed` (`Checked`) records (`typed`)**: canonical
//!    `checked_record::encode` bytes keyed by non-local `ItemKey`, so dependency/toolchain bodies
//!    not yet in the pre-walk store snapshot are typechecked at most once across all workers.
//!
//! Concurrency & determinism invariants:
//! - Every concurrent table is partitioned into [`SHARDS`] (64) 128-byte cache-line-padded shards
//!   guarded by named [`blaze_core::lock::ReadLock`]s (Rule 12), held ONLY for O(1) map lookup /
//!   `Arc::clone` / insert and NEVER across lowering, typechecking, or allocation-heavy work.
//! - Every table is strictly bounded (Rule 18): once a cap is reached, additional puts are cleanly
//!   dropped without panicking or growing unboundedly.
//! - Append-only, canonical-value semantics (Rule 25): every cached value is a pure function of
//!   its key and immutable crate records, so program output never depends on worker scheduling.
//! - Command-scoped lifetime (Rule 70): owned by the command and dropped cleanly at command end.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::alloc::Global;
use std::sync::Arc;

use blaze_build::data::{DataAlloc, DataRecord, DataReloc, DataTarget};
use blaze_build::StaticItem;
use blaze_core::lock::ReadLock;
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_id::{CrateItemKey, ItemKey as LibKey};
use blaze_own::{SharedAlloc, String as OwnString, Vec as OwnVec};
use blaze_typeck::ItemKey;

use crate::front2::keys::MemberKind;
use crate::front2::tables::head::WANT_BYTES;

/// Number of cache-line-padded shards per hot concurrent table. Power of two so shard selection is
/// a single bitmask with zero division and zero false sharing across 128-core ARM64 servers and
/// Apple Silicon M-series (128-byte cache lines).
pub const SHARDS: usize = 64;

/// Maximum frozen head cells retained in one command's [`SharedLazyFront`] (Rule 18).
pub const MAX_FROZEN_HEADS: usize = 512_000;
/// Maximum evaluated constant bit patterns (`LibKey -> u128`) retained (Rule 18).
pub const MAX_CONST_BITS: usize = 256_000;
/// Maximum pure constant [`DataRecord`]s retained (Rule 18).
pub const MAX_PURE_CONSTS: usize = 131_072;
/// Maximum pure static [`DataRecord`]s retained (Rule 18).
pub const MAX_PURE_STATICS: usize = 65_536;
/// Maximum freshly checked dependency/toolchain `Typed` records retained (Rule 18).
pub const MAX_TYPED_BODIES: usize = 131_072;
/// Maximum converged polymorphization usage summaries retained across parallel workers (Rule 18).
pub const MAX_POLY_USAGE: usize = 131_072;
/// Maximum per-crate member indexes retained (Rule 18).
pub const MAX_CRATE_MEMBER_INDEXES: usize = 4_096;

/// Immutable shared byte buffer allocated in [`SharedAlloc`], returned from shard lookups with an
/// O(1) atomic refcount increment so shard locks are dropped immediately.
pub type SharedBytes = Arc<OwnVec<u8, SharedAlloc<'static>>, SharedAlloc<'static>>;

/// 128-byte cache-line-padded wrapper preventing false sharing across 128-core NUMA/SMP servers
/// and Apple Silicon P/E clusters.
#[repr(align(128))]
struct CachePadded<T>(T);

impl<T> CachePadded<T> {
    const fn new(value: T) -> Self {
        Self(value)
    }
}

/// Snapshot of [`SharedLazyFront`] cache telemetry for `blaze build --stats` and tests.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct SharedLazyFrontStats {
    pub head_hits: u64,
    pub head_misses: u64,
    pub head_puts: u64,
    pub const_bit_hits: u64,
    pub const_bit_misses: u64,
    pub pure_const_hits: u64,
    pub pure_const_misses: u64,
    pub pure_static_hits: u64,
    pub pure_static_misses: u64,
    pub member_index_hits: u64,
    pub member_index_misses: u64,
    pub typed_hits: u64,
    pub typed_misses: u64,
    pub typed_puts: u64,
    pub poly_usage_hits: u64,
    pub poly_usage_misses: u64,
    pub poly_usage_puts: u64,
    pub poly_generic_calls: u64,
    pub poly_canonicalized_calls: u64,
}

struct ShardCounters {
    head_hits: CachePadded<AtomicU64>,
    head_misses: CachePadded<AtomicU64>,
    head_puts: CachePadded<AtomicU64>,
    const_bit_hits: CachePadded<AtomicU64>,
    const_bit_misses: CachePadded<AtomicU64>,
    pure_const_hits: CachePadded<AtomicU64>,
    pure_const_misses: CachePadded<AtomicU64>,
    pure_static_hits: CachePadded<AtomicU64>,
    pure_static_misses: CachePadded<AtomicU64>,
    member_index_hits: CachePadded<AtomicU64>,
    member_index_misses: CachePadded<AtomicU64>,
    typed_hits: CachePadded<AtomicU64>,
    typed_misses: CachePadded<AtomicU64>,
    typed_puts: CachePadded<AtomicU64>,
    poly_usage_hits: CachePadded<AtomicU64>,
    poly_usage_misses: CachePadded<AtomicU64>,
    poly_usage_puts: CachePadded<AtomicU64>,
    poly_generic_calls: CachePadded<AtomicU64>,
    poly_canonicalized_calls: CachePadded<AtomicU64>,
}

impl ShardCounters {
    const fn new() -> Self {
        Self {
            head_hits: CachePadded::new(AtomicU64::new(0)),
            head_misses: CachePadded::new(AtomicU64::new(0)),
            head_puts: CachePadded::new(AtomicU64::new(0)),
            const_bit_hits: CachePadded::new(AtomicU64::new(0)),
            const_bit_misses: CachePadded::new(AtomicU64::new(0)),
            pure_const_hits: CachePadded::new(AtomicU64::new(0)),
            pure_const_misses: CachePadded::new(AtomicU64::new(0)),
            pure_static_hits: CachePadded::new(AtomicU64::new(0)),
            pure_static_misses: CachePadded::new(AtomicU64::new(0)),
            member_index_hits: CachePadded::new(AtomicU64::new(0)),
            member_index_misses: CachePadded::new(AtomicU64::new(0)),
            typed_hits: CachePadded::new(AtomicU64::new(0)),
            typed_misses: CachePadded::new(AtomicU64::new(0)),
            typed_puts: CachePadded::new(AtomicU64::new(0)),
            poly_usage_hits: CachePadded::new(AtomicU64::new(0)),
            poly_usage_misses: CachePadded::new(AtomicU64::new(0)),
            poly_usage_puts: CachePadded::new(AtomicU64::new(0)),
            poly_generic_calls: CachePadded::new(AtomicU64::new(0)),
            poly_canonicalized_calls: CachePadded::new(AtomicU64::new(0)),
        }
    }

    fn snapshot(&self) -> SharedLazyFrontStats {
        SharedLazyFrontStats {
            head_hits: self.head_hits.0.load(Ordering::Relaxed),
            head_misses: self.head_misses.0.load(Ordering::Relaxed),
            head_puts: self.head_puts.0.load(Ordering::Relaxed),
            const_bit_hits: self.const_bit_hits.0.load(Ordering::Relaxed),
            const_bit_misses: self.const_bit_misses.0.load(Ordering::Relaxed),
            pure_const_hits: self.pure_const_hits.0.load(Ordering::Relaxed),
            pure_const_misses: self.pure_const_misses.0.load(Ordering::Relaxed),
            pure_static_hits: self.pure_static_hits.0.load(Ordering::Relaxed),
            pure_static_misses: self.pure_static_misses.0.load(Ordering::Relaxed),
            member_index_hits: self.member_index_hits.0.load(Ordering::Relaxed),
            member_index_misses: self.member_index_misses.0.load(Ordering::Relaxed),
            typed_hits: self.typed_hits.0.load(Ordering::Relaxed),
            typed_misses: self.typed_misses.0.load(Ordering::Relaxed),
            typed_puts: self.typed_puts.0.load(Ordering::Relaxed),
            poly_usage_hits: self.poly_usage_hits.0.load(Ordering::Relaxed),
            poly_usage_misses: self.poly_usage_misses.0.load(Ordering::Relaxed),
            poly_usage_puts: self.poly_usage_puts.0.load(Ordering::Relaxed),
            poly_generic_calls: self.poly_generic_calls.0.load(Ordering::Relaxed),
            poly_canonicalized_calls: self.poly_canonicalized_calls.0.load(Ordering::Relaxed),
        }
    }
}

/// Bounded append-only 64-shard concurrent map using named [`ReadLock`]s and explicit
/// [`SharedAlloc`] (Rules 11, 12, 18).
struct ShardedMap<K, V> {
    shards: OwnVec<CachePadded<ReadLock<Map<K, V, FixedState, SharedAlloc<'static>>>>, SharedAlloc<'static>>,
    count: CachePadded<AtomicUsize>,
    cap: usize,
}

impl<K: Eq + core::hash::Hash, V> ShardedMap<K, V> {
    fn new_in(lock_name: &'static str, cap: usize, alloc: SharedAlloc<'static>) -> Self {
        let mut shards = OwnVec::with_capacity_in(SHARDS, alloc);
        for _ in 0..SHARDS {
            let map = Map::new(MapOpts { hasher: FixedState, alloc, capacity: None });
            shards.push(CachePadded::new(ReadLock::new(lock_name, map)));
        }
        Self { shards, count: CachePadded::new(AtomicUsize::new(0)), cap }
    }

    #[inline]
    fn get_cloned(&self, shard: usize, key: &K) -> Option<V>
    where
        V: Clone,
    {
        let guard = self.shards[shard & (SHARDS - 1)].0.read();
        guard.get(key).cloned()
    }

    #[inline]
    fn contains(&self, shard: usize, key: &K) -> bool {
        let guard = self.shards[shard & (SHARDS - 1)].0.read();
        guard.contains_key(key)
    }

    /// Inserts `(key, value)` if absent and under [`Self::cap`]. Returns `true` iff newly inserted.
    fn insert_if_absent(&self, shard: usize, key: K, make_val: impl FnOnce() -> V) -> bool {
        // Fast pre-check under read lock avoids write-lock acquisition on duplicates.
        {
            let guard = self.shards[shard & (SHARDS - 1)].0.read();
            if guard.contains_key(&key) {
                return false;
            }
        }
        if self.count.0.load(Ordering::Relaxed) >= self.cap {
            return false;
        }
        let val = make_val();
        let mut guard = self.shards[shard & (SHARDS - 1)].0.write();
        if guard.contains_key(&key) {
            return false;
        }
        if self.count.0.load(Ordering::Relaxed) >= self.cap {
            return false;
        }
        guard.insert(key, val);
        self.count.0.fetch_add(1, Ordering::Relaxed);
        true
    }

    fn len(&self) -> usize {
        self.count.0.load(Ordering::Relaxed)
    }
}

/// Self-contained [`DataRecord`] allocation stored in [`SharedAlloc`] (only relocations targeting
/// `DataTarget::Local` or `DataTarget::Static`, never evaluator-local `Fn`/`Vtable` indices).
struct SharedDataAlloc {
    bytes: OwnVec<u8, SharedAlloc<'static>>,
    align: u8,
    mutable: bool,
    relocs: OwnVec<DataReloc, SharedAlloc<'static>>,
}

/// Immutable self-contained constant or static [`DataRecord`] shared across all workers.
pub struct SharedDataRecord {
    allocs: OwnVec<SharedDataAlloc, SharedAlloc<'static>>,
}

/// Returns `true` iff `record` has zero evaluator-local relocations (`DataTarget::Fn` or
/// `DataTarget::Vtable`), so its bytes + relocations can be reused safely on any worker without
/// consulting the producing worker's `Consts` table.
pub fn is_self_contained_data_record<A: core::alloc::Allocator>(record: &DataRecord<A>) -> bool {
    for alloc in record.allocs.iter() {
        for reloc in alloc.relocs.iter() {
            match reloc.target {
                DataTarget::Local(_) | DataTarget::Static(_) => {}
                DataTarget::Fn(_) | DataTarget::Vtable(_) => return false,
            }
        }
    }
    true
}

impl SharedDataRecord {
    fn from_record(record: &DataRecord<Global>, shared: SharedAlloc<'static>) -> Self {
        let mut allocs = OwnVec::with_capacity_in(record.allocs.len(), shared);
        for a in record.allocs.iter() {
            let mut bytes = OwnVec::with_capacity_in(a.bytes.len(), shared);
            bytes.extend_from_slice(a.bytes.as_slice());
            let mut relocs = OwnVec::with_capacity_in(a.relocs.len(), shared);
            relocs.extend_from_slice(a.relocs.as_slice());
            allocs.push(SharedDataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
        }
        Self { allocs }
    }

    /// Materializes a local `DataRecord<Global>` copy for a worker's `LazyProgram::const_values` /
    /// `statics` table.
    pub fn to_global_record(&self) -> DataRecord<Global> {
        let mut allocs = std::vec::Vec::with_capacity_in(self.allocs.len(), Global);
        for a in self.allocs.iter() {
            let mut bytes = std::vec::Vec::with_capacity_in(a.bytes.len(), Global);
            bytes.extend_from_slice(a.bytes.as_slice());
            let mut relocs = std::vec::Vec::with_capacity_in(a.relocs.len(), Global);
            relocs.extend_from_slice(a.relocs.as_slice());
            allocs.push(DataAlloc { bytes, align: a.align, mutable: a.mutable, relocs });
        }
        DataRecord { allocs }
    }
}

/// One indexed member row inside [`SharedMemberIndex`] referencing a slice of its flat name buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SharedMemberRow {
    pub container: LibKey,
    pub kind: MemberKind,
    name_at: u32,
    name_len: u32,
}

/// Immutable per-crate member index shared via `Arc` across all workers and worker epochs,
/// eliminating 128× redundant crate-wide `each_item` / `each_member_matching` scans.
pub(super) struct SharedMemberIndex {
    rows: Map<ItemKey, Option<SharedMemberRow>, FixedState, SharedAlloc<'static>>,
    names: OwnString<SharedAlloc<'static>>,
    pub containers: u32,
}

impl SharedMemberIndex {
    pub(super) fn new_in(containers: u32, alloc: SharedAlloc<'static>) -> Self {
        Self {
            rows: Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            names: OwnString::new_in(alloc),
            containers,
        }
    }

    pub(super) fn put(&mut self, key: ItemKey, container: LibKey, kind: MemberKind, name: &str) {
        if let Some(existing) = self.rows.get(&key).copied() {
            match existing {
                Some(first) => {
                    let same = first.container == container
                        && first.kind == kind
                        && self.name_of(first) == name;
                    if !same {
                        self.rows.insert(key, None);
                    }
                }
                None => {}
            }
            return;
        }
        let Ok(name_at) = u32::try_from(self.names.len()) else { return };
        let Ok(name_len) = u32::try_from(name.len()) else { return };
        self.names.push_str(name);
        self.rows.insert(key, Some(SharedMemberRow { container, kind, name_at, name_len }));
    }

    pub(super) fn get(&self, key: ItemKey) -> Option<Option<(LibKey, MemberKind, &str)>> {
        self.rows.get(&key).map(|opt| opt.map(|row| (row.container, row.kind, self.name_of(row))))
    }

    pub(super) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(super) fn find_member_fn(&self, mut f: impl FnMut(ItemKey) -> Option<(ItemKey, bool)>) -> Option<(ItemKey, bool)> {
        for (&fk, row) in self.rows.iter() {
            let Some(row) = *row else { continue };
            if row.kind != MemberKind::Fn {
                continue;
            }
            if let Some(hit) = f(fk) {
                return Some(hit);
            }
        }
        None
    }

    #[inline]
    fn name_of(&self, row: SharedMemberRow) -> &str {
        let start = row.name_at as usize;
        let end = start.saturating_add(row.name_len as usize);
        self.names.as_str().get(start..end).unwrap_or("")
    }
}

/// Returns `true` iff checker key `key` is a body-local item (`LibKey::INNER` set in byte 8).
#[inline]
pub fn is_body_local(key: ItemKey) -> bool {
    key.0.to_be_bytes()[8] & LibKey::INNER != 0
}

#[inline]
fn shard_of_want(key: &[u8; WANT_BYTES]) -> usize {
    // Bytes 0 is tag, bytes 1..9 are crate prefix, bytes 9..33 are item hash, bytes 33..49 are
    // member key hash. Mix high-entropy hash bytes so items in the same crate spread across all
    // 64 shards uniformly.
    (usize::from(key[0]) ^ usize::from(key[9]) ^ (usize::from(key[10]) << 1) ^ usize::from(key[33]) ^ (usize::from(key[34]) << 2))
        & (SHARDS - 1)
}

#[inline]
fn shard_of_lib_key(key: LibKey) -> usize {
    // Bytes 0..8 are crate prefix; bytes 8..32 are item hash.
    (usize::from(key.0.0[8]) ^ (usize::from(key.0.0[9]) << 1) ^ (usize::from(key.0.0[10]) << 2)) & (SHARDS - 1)
}

#[inline]
fn shard_of_item_key(key: ItemKey) -> usize {
    // High 64 bits are crate prefix; low 64 bits are item hash.
    let lo = key.0 as u64;
    ((lo as usize) ^ ((lo >> 8) as usize) ^ ((lo >> 16) as usize)) & (SHARDS - 1)
}

/// Immutable converged polymorphization summary shared across parallel workers for one non-local
/// generic function `ItemKey`.
pub struct SharedPolyUsage {
    pub usage: OwnVec<crate::front2::poly_canon::ParamUse, SharedAlloc<'static>>,
    /// Function items (self + any analyzed transitive callees) whose `ReadKind::PolyBodyText`
    /// must be logged whenever a caller canonicalizes a call to this function.
    pub deps: OwnVec<ItemKey, SharedAlloc<'static>>,
    /// ADTs whose layout/drop summaries contributed to `usage` (so callers log `ReadKind::Adt`
    /// / `ReadKind::DropImpl` when canonicalizing).
    pub adt_deps: OwnVec<ItemKey, SharedAlloc<'static>>,
    pub can_canon: bool,
}

/// Command-scoped shared immutable/sharded front-end cache shared across all parallel export
/// workers and across worker epoch resets (module docs).
pub struct SharedLazyFront {
    alloc: SharedAlloc<'static>,
    heads: ShardedMap<[u8; WANT_BYTES], SharedBytes>,
    const_bits: ShardedMap<LibKey, u128>,
    pure_consts: ShardedMap<ItemKey, Arc<SharedDataRecord, SharedAlloc<'static>>>,
    pure_statics: ShardedMap<ItemKey, (Arc<SharedDataRecord, SharedAlloc<'static>>, StaticItem)>,
    typed: ShardedMap<ItemKey, SharedBytes>,
    poly_usage: ShardedMap<ItemKey, Arc<SharedPolyUsage, SharedAlloc<'static>>>,
    crate_members: CachePadded<ReadLock<Map<CrateItemKey, Arc<SharedMemberIndex, SharedAlloc<'static>>, FixedState, SharedAlloc<'static>>>>,
    lang_members: CachePadded<ReadLock<Map<CrateItemKey, Arc<SharedMemberIndex, SharedAlloc<'static>>, FixedState, SharedAlloc<'static>>>>,
    counters: ShardCounters,
}

// Compile-time proof that `SharedLazyFront` is `Send + Sync` for multi-threaded worker sharing.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SharedLazyFront>();
};

impl SharedLazyFront {
    /// Creates an empty command-scoped [`SharedLazyFront`] backed by explicit `alloc` (Rule 11).
    pub fn new_in(alloc: SharedAlloc<'static>) -> Self {
        Self::with_caps_in(
            alloc,
            MAX_FROZEN_HEADS,
            MAX_CONST_BITS,
            MAX_PURE_CONSTS,
            MAX_PURE_STATICS,
            MAX_TYPED_BODIES,
        )
    }

    /// Creates a [`SharedLazyFront`] with explicit per-table entry caps (for boundedness tests).
    pub(crate) fn with_caps_in(
        alloc: SharedAlloc<'static>,
        max_heads: usize,
        max_const_bits: usize,
        max_pure_consts: usize,
        max_pure_statics: usize,
        max_typed: usize,
    ) -> Self {
        Self {
            alloc,
            heads: ShardedMap::new_in("lazy.shared.heads", max_heads, alloc),
            const_bits: ShardedMap::new_in("lazy.shared.const_bits", max_const_bits, alloc),
            pure_consts: ShardedMap::new_in("lazy.shared.pure_consts", max_pure_consts, alloc),
            pure_statics: ShardedMap::new_in("lazy.shared.pure_statics", max_pure_statics, alloc),
            typed: ShardedMap::new_in("lazy.shared.typed", max_typed, alloc),
            poly_usage: ShardedMap::new_in("lazy.shared.poly_usage", MAX_POLY_USAGE, alloc),
            crate_members: CachePadded::new(ReadLock::new(
                "lazy.shared.crate_members",
                Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            )),
            lang_members: CachePadded::new(ReadLock::new(
                "lazy.shared.lang_members",
                Map::new(MapOpts { hasher: FixedState, alloc, capacity: None }),
            )),
            counters: ShardCounters::new(),
        }
    }

    /// Shared allocator used by this front-end cache.
    #[inline]
    pub fn alloc(&self) -> SharedAlloc<'static> {
        self.alloc
    }

    /// Snapshot of hit/miss/put telemetry across all shared tables.
    pub fn stats(&self) -> SharedLazyFrontStats {
        self.counters.snapshot()
    }

    /// Number of distinct frozen head cells currently stored.
    pub fn heads_len(&self) -> usize {
        self.heads.len()
    }

    /// Number of distinct `const_bits` entries currently stored.
    pub fn const_bits_len(&self) -> usize {
        self.const_bits.len()
    }

    /// Number of distinct pure constant [`DataRecord`]s currently stored.
    pub fn pure_consts_len(&self) -> usize {
        self.pure_consts.len()
    }

    /// Number of distinct `Typed` records currently stored.
    pub fn typed_len(&self) -> usize {
        self.typed.len()
    }

    // ---- Frozen heads (`BZFZ`) ----

    /// Looks up a previously frozen `BZFZ` head cell by `want_bytes(cell)`.
    pub fn get_head(&self, key: &[u8; WANT_BYTES]) -> Option<SharedBytes> {
        let found = self.heads.get_cloned(shard_of_want(key), key);
        if found.is_some() {
            self.counters.head_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.head_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Fast read-only probe used before encoding a freshly lowered cell.
    pub fn contains_head(&self, key: &[u8; WANT_BYTES]) -> bool {
        self.heads.contains(shard_of_want(key), key)
    }

    /// Publishes canonical `BZFZ` frozen head bytes for `key`. Returns `true` if newly inserted.
    pub fn put_head(&self, key: [u8; WANT_BYTES], bytes: &[u8]) -> bool {
        let alloc = self.alloc;
        let inserted = self.heads.insert_if_absent(shard_of_want(&key), key, || {
            let mut buf = OwnVec::with_capacity_in(bytes.len(), alloc);
            buf.extend_from_slice(bytes);
            Arc::new_in(buf, alloc)
        });
        if inserted {
            self.counters.head_puts.0.fetch_add(1, Ordering::Relaxed);
        }
        inserted
    }

    // ---- Evaluated const-generic / array-length bit patterns ----

    pub fn get_const_bit(&self, key: LibKey) -> Option<u128> {
        let found = self.const_bits.get_cloned(shard_of_lib_key(key), &key);
        if found.is_some() {
            self.counters.const_bit_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.const_bit_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    pub fn put_const_bit(&self, key: LibKey, bits: u128) -> bool {
        self.const_bits.insert_if_absent(shard_of_lib_key(key), key, || bits)
    }

    // ---- Pure evaluated constant `DataRecord`s ----

    pub fn get_pure_const(&self, key: ItemKey) -> Option<DataRecord<Global>> {
        if is_body_local(key) {
            return None;
        }
        let found = self.pure_consts.get_cloned(shard_of_item_key(key), &key);
        if let Some(rec) = found {
            self.counters.pure_const_hits.0.fetch_add(1, Ordering::Relaxed);
            Some(rec.to_global_record())
        } else {
            self.counters.pure_const_misses.0.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    pub fn put_pure_const(&self, key: ItemKey, record: &DataRecord<Global>) -> bool {
        if is_body_local(key) || !is_self_contained_data_record(record) {
            return false;
        }
        let alloc = self.alloc;
        self.pure_consts.insert_if_absent(shard_of_item_key(key), key, || {
            Arc::new_in(SharedDataRecord::from_record(record, alloc), alloc)
        })
    }

    // ---- Pure evaluated static `DataRecord`s ----

    pub fn get_pure_static(&self, key: ItemKey) -> Option<(DataRecord<Global>, StaticItem)> {
        if is_body_local(key) {
            return None;
        }
        let found = self.pure_statics.get_cloned(shard_of_item_key(key), &key);
        if let Some((rec, kind)) = found {
            self.counters.pure_static_hits.0.fetch_add(1, Ordering::Relaxed);
            Some((rec.to_global_record(), kind))
        } else {
            self.counters.pure_static_misses.0.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    pub fn put_pure_static(&self, key: ItemKey, record: &DataRecord<Global>, kind: StaticItem) -> bool {
        if is_body_local(key) || !is_self_contained_data_record(record) {
            return false;
        }
        let alloc = self.alloc;
        self.pure_statics.insert_if_absent(shard_of_item_key(key), key, || {
            (Arc::new_in(SharedDataRecord::from_record(record, alloc), alloc), kind)
        })
    }

    // ---- Shared per-crate member & lang-member indexes ----

    pub(super) fn get_crate_members(&self, krate: CrateItemKey) -> Option<Arc<SharedMemberIndex, SharedAlloc<'static>>> {
        let guard = self.crate_members.0.read();
        let found = guard.get(&krate).cloned();
        drop(guard);
        if found.is_some() {
            self.counters.member_index_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.member_index_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    pub(super) fn put_crate_members(
        &self,
        krate: CrateItemKey,
        ix: SharedMemberIndex,
    ) -> Arc<SharedMemberIndex, SharedAlloc<'static>> {
        let mut guard = self.crate_members.0.write();
        if let Some(existing) = guard.get(&krate) {
            return Arc::clone(existing);
        }
        let shared = Arc::new_in(ix, self.alloc);
        if guard.len() < MAX_CRATE_MEMBER_INDEXES {
            guard.insert(krate, Arc::clone(&shared));
        }
        shared
    }

    pub(super) fn get_lang_members(&self, krate: CrateItemKey) -> Option<Arc<SharedMemberIndex, SharedAlloc<'static>>> {
        let guard = self.lang_members.0.read();
        let found = guard.get(&krate).cloned();
        drop(guard);
        if found.is_some() {
            self.counters.member_index_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.member_index_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    pub(super) fn put_lang_members(
        &self,
        krate: CrateItemKey,
        ix: SharedMemberIndex,
    ) -> Arc<SharedMemberIndex, SharedAlloc<'static>> {
        let mut guard = self.lang_members.0.write();
        if let Some(existing) = guard.get(&krate) {
            return Arc::clone(existing);
        }
        let shared = Arc::new_in(ix, self.alloc);
        if guard.len() < MAX_CRATE_MEMBER_INDEXES {
            guard.insert(krate, Arc::clone(&shared));
        }
        shared
    }

    // ---- Shared on-the-fly checked dependency/toolchain `Typed` (`Checked`) records ----

    /// Looks up canonical `checked_record::encode` bytes for non-local body `key`.
    pub fn get_typed_bytes(&self, key: ItemKey) -> Option<SharedBytes> {
        if is_body_local(key) {
            return None;
        }
        let found = self.typed.get_cloned(shard_of_item_key(key), &key);
        if found.is_some() {
            self.counters.typed_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.typed_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Publishes canonical `checked_record::encode` bytes for non-local body `key`.
    /// Returns `true` iff this caller was the first to insert `key`.
    pub fn put_typed_bytes(&self, key: ItemKey, bytes: &[u8]) -> bool {
        if is_body_local(key) {
            return false;
        }
        let alloc = self.alloc;
        let inserted = self.typed.insert_if_absent(shard_of_item_key(key), key, || {
            let mut buf = OwnVec::with_capacity_in(bytes.len(), alloc);
            buf.extend_from_slice(bytes);
            Arc::new_in(buf, alloc)
        });
        if inserted {
            self.counters.typed_puts.0.fetch_add(1, Ordering::Relaxed);
        }
        inserted
    }

    // ---- Shared converged polymorphization parameter-usage summaries ----

    /// Looks up the converged polymorphization summary for non-local generic function `key`.
    pub fn get_poly_usage(&self, key: ItemKey) -> Option<Arc<SharedPolyUsage, SharedAlloc<'static>>> {
        if is_body_local(key) {
            return None;
        }
        let found = self.poly_usage.get_cloned(shard_of_item_key(key), &key);
        if found.is_some() {
            self.counters.poly_usage_hits.0.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.poly_usage_misses.0.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    /// Publishes the converged polymorphization summary for non-local generic function `key`.
    pub fn put_poly_usage(
        &self,
        key: ItemKey,
        usage: &[crate::front2::poly_canon::ParamUse],
        deps: &[ItemKey],
        adt_deps: &[ItemKey],
        can_canon: bool,
    ) -> bool {
        if is_body_local(key) {
            return false;
        }
        let alloc = self.alloc;
        let inserted = self.poly_usage.insert_if_absent(shard_of_item_key(key), key, || {
            let mut u = OwnVec::with_capacity_in(usage.len(), alloc);
            u.extend_from_slice(usage);
            let mut d = OwnVec::with_capacity_in(deps.len(), alloc);
            d.extend_from_slice(deps);
            let mut a = OwnVec::with_capacity_in(adt_deps.len(), alloc);
            a.extend_from_slice(adt_deps);
            Arc::new_in(SharedPolyUsage { usage: u, deps: d, adt_deps: a, can_canon }, alloc)
        });
        if inserted {
            self.counters.poly_usage_puts.0.fetch_add(1, Ordering::Relaxed);
        }
        inserted
    }

    /// Records one generic call-site canonicalization attempt across parallel workers.
    #[inline]
    pub fn record_poly_call(&self, canonicalized: bool) {
        self.counters.poly_generic_calls.0.fetch_add(1, Ordering::Relaxed);
        if canonicalized {
            self.counters.poly_canonicalized_calls.0.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blaze_build::InstanceKey;
    use blaze_ids::ContentId;

    fn make_pure_record(val: u8) -> DataRecord<Global> {
        let mut bytes = std::vec::Vec::new_in(Global);
        bytes.extend_from_slice(&[val; 16]);
        let mut relocs = std::vec::Vec::new_in(Global);
        relocs.push(DataReloc { offset: 0, target: DataTarget::Local(0) });
        relocs.push(DataReloc { offset: 8, target: DataTarget::Static(ItemKey(0x1111)) });
        let mut allocs = std::vec::Vec::new_in(Global);
        allocs.push(DataAlloc { bytes, align: 3, mutable: false, relocs });
        DataRecord { allocs }
    }

    fn make_fn_reloc_record() -> DataRecord<Global> {
        let mut bytes = std::vec::Vec::new_in(Global);
        bytes.extend_from_slice(&[0u8; 8]);
        let mut relocs = std::vec::Vec::new_in(Global);
        relocs.push(DataReloc { offset: 0, target: DataTarget::Fn(InstanceKey(ContentId([7; 32]))) });
        let mut allocs = std::vec::Vec::new_in(Global);
        allocs.push(DataAlloc { bytes, align: 3, mutable: false, relocs });
        DataRecord { allocs }
    }

    #[test]
    fn pure_const_filter_rejects_fn_relocs_and_body_locals() {
        let shared = SharedLazyFront::new_in(blaze_own::testing::heap().shared());
        let pure = make_pure_record(0xAB);
        let with_fn = make_fn_reloc_record();
        assert!(is_self_contained_data_record(&pure));
        assert!(!is_self_contained_data_record(&with_fn));

        let global_key = ItemKey(0x0000_0000_0000_0001_0000_0000_0000_0042);
        assert!(!is_body_local(global_key));
        assert!(shared.put_pure_const(global_key, &pure));
        assert!(!shared.put_pure_const(global_key, &pure), "duplicate insert returns false");
        let got = shared.get_pure_const(global_key).expect("pure const hit");
        assert_eq!(got.allocs.len(), 1);
        assert_eq!(got.allocs[0].bytes.as_slice(), &[0xAB; 16]);
        assert_eq!(got.allocs[0].relocs.len(), 2);

        let fn_key = ItemKey(0x0000_0000_0000_0001_0000_0000_0000_0043);
        assert!(!shared.put_pure_const(fn_key, &with_fn), "evaluator-local Fn reloc must not be cached");
        assert!(shared.get_pure_const(fn_key).is_none());

        // Set `LibKey::INNER` bit in byte 8.
        let mut local_bytes = global_key.0.to_be_bytes();
        local_bytes[8] |= LibKey::INNER;
        let local_key = ItemKey(u128::from_be_bytes(local_bytes));
        assert!(is_body_local(local_key));
        assert!(!shared.put_pure_const(local_key, &pure), "body-local const must never be shared");
        assert!(!shared.put_typed_bytes(local_key, b"typed"), "body-local typed record must never be shared");
    }

    #[test]
    fn shared_front_enforces_bounded_caps() {
        let shared = SharedLazyFront::with_caps_in(blaze_own::testing::heap().shared(), 4, 3, 2, 2, 2);
        for i in 0..10u8 {
            let mut key = [0u8; WANT_BYTES];
            key[0] = 1;
            key[9] = i;
            let _ = shared.put_head(key, &[i; 8]);
            let lib = LibKey(ContentId([i; 32]));
            let _ = shared.put_const_bit(lib, u128::from(i));
            let ik = ItemKey(u128::from(i) + 1);
            let _ = shared.put_pure_const(ik, &make_pure_record(i));
            let _ = shared.put_typed_bytes(ik, &[i; 4]);
        }
        assert_eq!(shared.heads_len(), 4, "heads table must respect cap");
        assert_eq!(shared.const_bits_len(), 3, "const_bits table must respect cap");
        assert_eq!(shared.pure_consts_len(), 2, "pure_consts table must respect cap");
        assert_eq!(shared.typed_len(), 2, "typed table must respect cap");
    }

    #[test]
    fn shared_member_index_handles_lookup_and_collision() {
        let alloc = blaze_own::testing::heap().shared();
        let mut ix = SharedMemberIndex::new_in(2, alloc);
        let c1 = LibKey(ContentId([1; 32]));
        let c2 = LibKey(ContentId([2; 32]));
        let k1 = ItemKey(101);
        let k2 = ItemKey(202);
        ix.put(k1, c1, MemberKind::Fn, "push");
        ix.put(k1, c1, MemberKind::Fn, "push"); // idempotent duplicate
        assert_eq!(ix.get(k1), Some(Some((c1, MemberKind::Fn, "push"))));

        // Conflicting second member under the same key marks collision (`Some(None)`).
        ix.put(k2, c1, MemberKind::Fn, "foo");
        ix.put(k2, c2, MemberKind::Fn, "bar");
        assert_eq!(ix.get(k2), Some(None));
        assert_eq!(ix.get(ItemKey(999)), None);
    }

    #[test]
    fn shared_front_multithreaded_concurrent_inserts_and_reads_are_deterministic() {
        let alloc = blaze_own::testing::heap().shared();
        let shared = Arc::new_in(SharedLazyFront::new_in(alloc), alloc);
        let threads = 8usize;
        let items = 128usize;

        std::thread::scope(|scope| {
            for t in 0..threads {
                let s = Arc::clone(&shared);
                scope.spawn(move || {
                    for i in 0..items {
                        let idx = (i + t * 17) % items;
                        let mut head_key = [0u8; WANT_BYTES];
                        head_key[0] = 2;
                        head_key[9..17].copy_from_slice(&(idx as u64).to_le_bytes());
                        let payload = (idx as u64).to_le_bytes();
                        let _ = s.put_head(head_key, &payload);
                        let got = s.get_head(&head_key).expect("head present after put");
                        assert_eq!(got.as_slice(), &payload);

                        let ik = ItemKey((idx as u128) + 1000);
                        let _ = s.put_typed_bytes(ik, &payload);
                        let got_t = s.get_typed_bytes(ik).expect("typed present after put");
                        assert_eq!(got_t.as_slice(), &payload);
                    }
                });
            }
        });

        let stats = shared.stats();
        assert_eq!(shared.heads_len(), items);
        assert_eq!(shared.typed_len(), items);
        assert_eq!(stats.head_puts, items as u64, "each head key inserted exactly once");
        assert_eq!(stats.typed_puts, items as u64, "each typed key inserted exactly once");
        assert_eq!(stats.head_hits, (threads * items) as u64);
        assert_eq!(stats.typed_hits, (threads * items) as u64);
    }
}
