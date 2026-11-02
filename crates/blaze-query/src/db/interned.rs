//! Interned structs: one row of fields per distinct value, found by a lock-free probe of a
//! shard's [`swiss::Index`] (as hir-ty's interner): the hit is the shard's level, its home
//! group's word and slot, then the row's hash and fields; no lock, no write, no edge. A miss
//! takes the shard's lock, probes again, and writes the row before publishing its ID.

use std::{
    cell::UnsafeCell,
    hash::{BuildHasher, Hash},
    marker::PhantomData,
    mem::MaybeUninit,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use blaze_hash::FixedState;
use blaze_mem::{Tag, swiss};
use parking_lot::Mutex;

use super::{
    DatabaseKeyIndex, EventKind, Id, MemoryUsage,
    ingredient::{Ingredient, Kind, Vtable},
    input::check_owner,
    row::{Pages, layout},
    storage::{Ids, Storage, Table},
};

/// One interned struct. `Fields` with `'db` erased to `'static`; equality and hash are the
/// fields' own.
pub trait Interned: 'static {
    type Fields: Eq + Hash + Send + Sync + 'static;
    const NAME: &'static str;
    const FILE: &'static str;
    fn ingredient() -> &'static Ingredient;
    /// Each `AsId` field's ID and what each `NamesIds` field names, in field order.
    fn ids_of(fields: &Self::Fields, out: &mut Vec<Id>);
}

/// What an interned field names besides itself being an ID.
pub trait NamesIds {
    fn names_ids(&self, out: &mut Vec<Id>);
}

/// The suffix of a function's hidden key's name.
const ARGUMENTS: &str = "::interned_arguments";

const SHARDS: usize = 64;

/// One interned value. `hash` and `fields` are written before the ID is published in the
/// index (release), and never again.
pub(crate) struct InternRow<F> {
    hash: AtomicU64,
    made: AtomicU32,
    fields: UnsafeCell<MaybeUninit<F>>,
}

struct Shard {
    index: swiss::Index,
    /// How many values the shard's index holds; its lock serializes inserts.
    len: Mutex<usize>,
}

pub(crate) struct InternTable<F> {
    pages: Pages,
    ids: Ids,
    shards: Box<[Shard; SHARDS]>,
    _of: PhantomData<fn() -> F>,
}

impl<F: Send + Sync + 'static> Table for InternTable<F> {
    fn new(_: &Storage, _: u32) -> InternTable<F> {
        InternTable {
            pages: Pages::new(Tag::Intern),
            ids: Ids::new(Tag::Intern),
            // alloc: once per interned ingredient per storage
            shards: Box::new(std::array::from_fn(|_| Shard {
                index: swiss::Index::new(),
                len: Mutex::new(0),
            })),
            _of: PhantomData,
        }
    }

    fn pages(&self) -> &Pages {
        &self.pages
    }
}

impl<F> InternTable<F> {
    #[inline]
    fn row(&self, id: Id) -> &InternRow<F> {
        self.pages
            .row::<InternRow<F>>(id)
            .expect("an interned ID's row page was made before the ID was given")
    }
}

impl Vtable {
    pub const fn interned<I: Interned>() -> Vtable {
        Vtable {
            name: I::NAME,
            file: I::FILE,
            kind: Kind::Interned,
            fields: &[],
            row_size: size_of::<InternRow<I::Fields>>(),
            ids_of: Some(ids_of::<I> as fn(&Storage, u32, Id, &mut Vec<Id>)),
            made: Some(made::<I> as fn(&Storage, u32, Id) -> bool),
            memory: memory::<I>,
            verify: None,
            reset: None,
            sweep: None,
            relocate: None,
            restamp: None,
            drop: drop_table::<I>,
        }
    }
}

/// The hash every interned value is keyed by: what [`intern_by`]'s `Q` must hash like.
#[inline]
pub(crate) fn hash_of<Q: Hash + ?Sized>(value: &Q) -> u64 {
    FixedState.hash_one(value)
}

#[inline]
fn shard(hash: u64) -> usize {
    (hash >> 51) as usize & (SHARDS - 1)
}

/// The ID of `fields`, interned once.
#[inline]
pub fn intern<I: Interned>(storage: &Storage, fields: I::Fields) -> Id {
    let hash = hash_of(&fields);
    intern_keyed::<I::Fields, I::Fields>(
        storage,
        I::ingredient(),
        hash,
        fields,
        |it, fields| it == fields,
        |fields| fields,
    )
    .0
}

/// The ID of the value `eq` accepts for `key`, made from `key` by `make` when new: a lookup by
/// a borrowed form, which hashes exactly as `Fields` does.
#[inline]
pub fn intern_by<I: Interned, Q: Hash>(
    storage: &Storage,
    key: Q,
    eq: impl Fn(&Q, &I::Fields) -> bool,
    make: impl FnOnce(Q) -> I::Fields,
) -> Id {
    let hash = hash_of(&key);
    intern_keyed::<I::Fields, Q>(
        storage,
        I::ingredient(),
        hash,
        key,
        |fields, key| eq(key, fields),
        make,
    )
    .0
}

/// The ID of the row `eq` finds for `key`, else of a new row `make` builds from `key`; the
/// key back when a row was found.
#[inline]
pub(crate) fn intern_keyed<F: Send + Sync + 'static, K>(
    storage: &Storage,
    ingredient: &'static Ingredient,
    hash: u64,
    key: K,
    eq: impl Fn(&F, &K) -> bool,
    make: impl FnOnce(K) -> F,
) -> (Id, Option<K>) {
    let (table, index) = storage.table_or_make::<InternTable<F>>(ingredient);
    let shard = &table.shards[shard(hash)];
    if let Some(id) = find(table, shard, hash, |row| eq(row, &key)) {
        return (id, Some(key));
    }
    insert(storage, table, index, shard, hash, key, eq, make)
}

#[inline]
fn find<F>(
    table: &InternTable<F>,
    shard: &Shard,
    hash: u64,
    eq: impl Fn(&F) -> bool,
) -> Option<Id> {
    shard
        .index
        .find(hash, |at| {
            let row = table.row(Id::from_u32(at));
            // SAFETY: the index publishes an ID (release, `swiss::Index::insert`) only after
            // its row's hash and fields were written, never again; `find` acquires it.
            row.hash.load(Ordering::Relaxed) == hash
                && eq(unsafe { (*row.fields.get()).assume_init_ref() })
        })
        .map(Id::from_u32)
}

#[cold]
#[inline(never)]
#[expect(
    clippy::too_many_arguments,
    reason = "the cold half of `intern_keyed`, which passes its own"
)]
fn insert<F, K>(
    storage: &Storage,
    table: &InternTable<F>,
    index: u32,
    shard: &Shard,
    hash: u64,
    key: K,
    eq: impl Fn(&F, &K) -> bool,
    make: impl FnOnce(K) -> F,
) -> (Id, Option<K>) {
    let mut len = shard.len.lock();
    if let Some(id) = find(table, shard, hash, |row| eq(row, &key)) {
        return (id, Some(key));
    }
    let fields = make(key);
    let id = table.ids.next(
        storage,
        index,
        &table.pages,
        &storage.intern_heap,
        layout::<InternRow<F>>(),
    );
    let row = table.row(id);
    row.hash.store(hash, Ordering::Relaxed);
    // SAFETY: the ID is fresh, so this task is its row's one writer, and no reader sees the
    // row before the index publishes it below.
    unsafe { (*row.fields.get()).write(fields) };
    row.made.store(1, Ordering::Relaxed);
    shard.index.insert(*len, hash, id.as_u32(), |at| {
        table.row(Id::from_u32(at)).hash.load(Ordering::Relaxed)
    });
    *len += 1;
    drop(len);
    storage.event(|| EventKind::DidInternValue { key: DatabaseKeyIndex::new(index, id) });
    (id, None)
}

/// The fields of interned `id`.
pub fn interned_fields<'s, I: Interned>(storage: &'s Storage, id: Id) -> &'s I::Fields {
    let index = I::ingredient().index();
    check_owner(storage, id, index, I::NAME);
    fields_of::<I::Fields>(storage, index, id)
        .unwrap_or_else(|| panic!("{}({id:?}) was never interned", I::NAME))
}

pub(crate) fn fields_of<F: Send + Sync + 'static>(
    storage: &Storage,
    index: u32,
    id: Id,
) -> Option<&F> {
    let row = storage.table::<InternTable<F>>(index)?.pages.row::<InternRow<F>>(id)?;
    // Acquire: an ID read from elsewhere (a field, a persisted `u32`) may come without the
    // index's ordering; `made` orders the fields before it.
    if row.made.load(Ordering::Acquire) == 0 {
        return None;
    }
    // SAFETY: made: written once before `made`, never again.
    Some(unsafe { (*row.fields.get()).assume_init_ref() })
}

/// The name of the function whose hidden key `id` is, if it is one.
pub fn interned_arguments(storage: &Storage, id: Id) -> Option<&'static str> {
    storage.ingredient_name(storage.owner(id)?).strip_suffix(ARGUMENTS)
}

fn made<I: Interned>(storage: &Storage, index: u32, id: Id) -> bool {
    fields_of::<I::Fields>(storage, index, id).is_some()
}

fn ids_of<I: Interned>(storage: &Storage, index: u32, id: Id, out: &mut Vec<Id>) {
    if let Some(fields) = fields_of::<I::Fields>(storage, index, id) {
        I::ids_of(fields, out);
    }
}

fn memory<I: Interned>(storage: &Storage, index: u32) -> MemoryUsage {
    memory_of::<I::Fields>(storage, index, I::NAME)
}

pub(crate) fn memory_of<F: Send + Sync + 'static>(
    storage: &Storage,
    index: u32,
    name: &'static str,
) -> MemoryUsage {
    let rows = storage.table::<InternTable<F>>(index).map_or(0, |table| u64::from(table.ids.len()));
    MemoryUsage {
        name,
        rows,
        row_bytes: rows * size_of::<InternRow<F>>() as u64,
        value_bytes: rows * size_of::<F>() as u64,
    }
}

pub(crate) fn drop_rows<F: Send + Sync + 'static>(storage: &mut Storage, index: u32) {
    let Some(table) = storage.take_table::<InternTable<F>>(index) else { return };
    for id in table.ids.all() {
        if let Some(row) = table.pages.row::<InternRow<F>>(id)
            && row.made.load(Ordering::Acquire) != 0
        {
            // SAFETY: the storage is being dropped: nothing reads the fields.
            unsafe { (*row.fields.get()).assume_init_drop() };
        }
    }
}

fn drop_table<I: Interned>(storage: &mut Storage, index: u32) {
    drop_rows::<I::Fields>(storage, index);
}
