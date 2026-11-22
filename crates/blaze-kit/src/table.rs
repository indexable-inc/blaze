//! [`Table`]: a vector indexed only by its own typed id, with [`TableSet`] (a bit per id) and
//! [`TableMap`] (a dense side table that grows with the id), and [`Branded`] reads for hot
//! loops: an index proven in range once, read with no bounds check after.
//!
//! An id of one table cannot index another: `Table<Local, _, _>` takes a `Local` and nothing
//! else, where `Vec<T>[usize]` accepts any integer (`refusals/wrong_table_index.txt`).
//!
//! # Floor
//!
//! - `Table`: `size_of::<T>()` bytes per entry, the vector's own; `get`/`at` is one compare and
//!   one load, one cache line. `push` past `2^32 - 2` entries is refused (ids are below
//!   `u32::MAX`, so `Option<Id>` is 4 bytes): `AllocFailure::CapacityOverflow` naming the table.
//! - `TableSet`: one bit per id up to the largest inserted; an operation touches one word.
//! - `TableMap`: `size_of::<Option<V>>()` per id up to the largest inserted (no hash, no
//!   key stored); an operation touches one slot. A map that holds few of many ids is a hash
//!   map's job instead.
//!
//! # Branded reads
//!
//! [`Table::checked`] lends the table's entries to a closure as a [`Branded`] view with a fresh
//! brand `'b`. [`Branded::check`] turns an id into an [`Ix<'b, I>`] after one compare against
//! the length, and [`Branded::get`] reads through it with no compare. Soundness, in full:
//!
//! 1. `'b` is a higher-ranked lifetime of the closure, invariant (`fn(&'b ()) -> &'b ()`), so
//!    it equals no other brand and no lifetime outside the closure: an `Ix<'b, _>` cannot be
//!    used with another `Branded`, nor kept past the closure (its type names `'b`, which the
//!    result type and every captured variable cannot) — GhostCell's argument (Yanovski et al.,
//!    ICFP 2021).
//! 2. `Ix` has private fields; the only ways to make one are `check` (which compares the index
//!    with the view's length) and `ids` (which yields indices below it).
//! 3. The view holds `&'t [T]`, a shared borrow of the table's entries for the whole closure, so
//!    the length cannot change while any `Ix<'b, _>` exists.
//!
//! So every `Ix<'b, _>` is below the length of the one slice its `Branded<'b>` reads, and the
//! read is in bounds.
//!
//! # Measured (`examples/table.rs`, release, one Mac run, 2026-11-15)
//!
//! ns per branded read at three sizes: 0.24, 0.57, 3.34, against a checked slice index's 0.39,
//! 0.65, 3.61 and `Table::at`'s 0.47, 0.70, 4.31.

use core::{
    alloc::Allocator,
    any::type_name,
    fmt,
    marker::PhantomData,
    ops::{Index, IndexMut},
    slice,
};

use blaze_int::Int;
use blaze_own::{AllocFailure, Refused, Vec};

use crate::{
    error::{fail, overflow},
    id::Id,
    plant::{self, Plant},
};

/// The most entries a table (or the ids of a set or map) holds: ids are `0..MOST`, so the
/// largest is `u32::MAX - 2` and `u32::MAX` stays free for `Option`'s niche.
pub const MOST: u32 = u32::MAX - 1;

/// The raw id of position `at`, which is below a table's length and so below [`MOST`].
#[inline(always)]
fn raw_of(at: usize) -> u32 {
    // Positions are below the length, which `try_push` keeps at most `MOST`: the low 32 bits
    // are the position.
    at.truncate_to::<u32>()
}

/// Stops on an id past the end of a table, naming the table's type, the id and the length.
#[cold]
#[inline(never)]
#[track_caller]
fn out_of_range(table: &'static str, id: u32, len: usize) -> ! {
    panic!("blaze_kit: id {id} is past the end of `{table}`, which has {len} entries")
}

/// A vector of `T` in `A`, indexed only by its id type `I`.
pub struct Table<I: Id, T, A: Allocator> {
    values: Vec<T, A>,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, T, A: Allocator> Table<I, T, A> {
    /// An empty table; allocates nothing until it grows.
    #[inline]
    pub const fn new_in(alloc: A) -> Table<I, T, A> {
        Table { values: Vec::new_in(alloc), _id: PhantomData }
    }

    /// An empty table with room for `capacity` entries.
    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<Table<I, T, A>, AllocFailure> {
        Ok(Table { values: Vec::try_with_capacity_in(capacity, alloc)?, _id: PhantomData })
    }

    /// [`Table::try_with_capacity_in`], panicking with the refusal.
    pub fn with_capacity_in(capacity: usize, alloc: A) -> Table<I, T, A> {
        match Table::try_with_capacity_in(capacity, alloc) {
            Ok(table) => table,
            Err(failure) => fail(failure),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn capacity(&self) -> usize {
        self.values.capacity()
    }

    pub fn allocator(&self) -> &A {
        self.values.allocator()
    }

    /// The id the next push gets, or `None` when the table is full of ids.
    pub fn next_id(&self) -> Option<I> {
        let len = self.values.len();
        match len.narrow::<u32>() {
            Ok(raw) if raw < MOST => Some(I::from_u32(raw)),
            _ => None,
        }
    }

    /// Room for `additional` more entries.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        self.values.try_reserve(additional)
    }

    /// Appends `value` and gives its id; refused, with the value back, when the allocator has
    /// no room or the table already holds [`MOST`] entries.
    #[inline]
    pub fn try_push(&mut self, value: T) -> Result<I, Refused<T>> {
        let Some(id) = self.next_id() else {
            return Err(Refused { value, why: overflow(type_name::<Self>(), self.values.len(), 1) });
        };
        self.values.try_push(value)?;
        Ok(id)
    }

    /// [`Table::try_push`], panicking with the refusal.
    #[inline]
    pub fn push(&mut self, value: T) -> I {
        match self.try_push(value) {
            Ok(id) => id,
            Err(refused) => fail(refused.why),
        }
    }

    /// The entry `id` names, or `None` past the end.
    #[inline]
    pub fn get(&self, id: I) -> Option<&T> {
        if plant::on(Plant::TableGetPastEnd) && id.index() == self.values.len() {
            return self.values.last();
        }
        self.values.get(id.index())
    }

    #[inline]
    pub fn get_mut(&mut self, id: I) -> Option<&mut T> {
        self.values.get_mut(id.index())
    }

    /// The entry `id` names; an id past the end is a bug, and panics at the caller naming the
    /// table's type, the id and the length.
    #[inline]
    #[track_caller]
    pub fn at(&self, id: I) -> &T {
        match self.values.get(id.index()) {
            Some(value) => value,
            None => out_of_range(type_name::<Self>(), id.to_u32(), self.values.len()),
        }
    }

    /// As [`Table::at`], mutably.
    #[inline]
    #[track_caller]
    pub fn at_mut(&mut self, id: I) -> &mut T {
        let len = self.values.len();
        match self.values.get_mut(id.index()) {
            Some(value) => value,
            None => out_of_range(type_name::<Self>(), id.to_u32(), len),
        }
    }

    /// The last entry out, or `None` when empty. Its id is free again: the next push reuses it.
    pub fn pop(&mut self) -> Option<T> {
        self.values.pop()
    }

    /// Keeps the first `len` entries; ids from `len` on are free again.
    pub fn truncate(&mut self, len: usize) {
        self.values.truncate(len);
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// The entries in id order.
    pub fn iter(&self) -> slice::Iter<'_, T> {
        self.values.iter()
    }

    pub fn iter_mut(&mut self) -> slice::IterMut<'_, T> {
        self.values.iter_mut()
    }

    /// Each entry with its id, in id order.
    pub fn iter_with_ids(&self) -> impl ExactSizeIterator<Item = (I, &T)> + DoubleEndedIterator {
        self.values.iter().enumerate().map(|(at, value)| (I::from_u32(raw_of(at)), value))
    }

    /// Every id the table has, in order.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = I> + DoubleEndedIterator + use<I, T, A> {
        (0..raw_of(self.values.len())).map(I::from_u32)
    }

    /// The entries as a slice, positions equal to ids.
    pub fn as_slice(&self) -> &[T] {
        self.values.as_slice()
    }

    pub fn as_mut_slice(&mut self) -> &mut [T] {
        self.values.as_mut_slice()
    }

    /// Runs `read` with a [`Branded`] view of the entries: ids checked once against the length
    /// become [`Ix`]s that read with no bounds check (see the module).
    #[inline]
    pub fn checked<'t, R>(&'t self, read: impl for<'b> FnOnce(Branded<'b, 't, I, T>) -> R) -> R {
        read(Branded { values: self.values.as_slice(), _brand: PhantomData, _id: PhantomData })
    }
}

impl<I: Id, T, A: Allocator> Index<I> for Table<I, T, A> {
    type Output = T;

    #[inline]
    #[track_caller]
    fn index(&self, id: I) -> &T {
        self.at(id)
    }
}

impl<I: Id, T, A: Allocator> IndexMut<I> for Table<I, T, A> {
    #[inline]
    #[track_caller]
    fn index_mut(&mut self, id: I) -> &mut T {
        self.at_mut(id)
    }
}

impl<I: Id + fmt::Debug, T: fmt::Debug, A: Allocator> fmt::Debug for Table<I, T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter_with_ids()).finish()
    }
}

/// An invariant lifetime: the brand of one [`Branded`] view.
type Brand<'b> = PhantomData<fn(&'b ()) -> &'b ()>;

/// An id proven below the length of the [`Branded`] view of brand `'b` (see the module).
pub struct Ix<'b, I> {
    id: I,
    _brand: Brand<'b>,
}

impl<I: Copy> Clone for Ix<'_, I> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I: Copy> Copy for Ix<'_, I> {}

impl<I: Copy> Ix<'_, I> {
    /// The id, to keep past the view or use on another table (checked again there).
    #[inline(always)]
    pub fn id(self) -> I {
        self.id
    }
}

impl<I: fmt::Debug> fmt::Debug for Ix<'_, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.id.fmt(f)
    }
}

/// A table's entries lent for one closure, under the brand `'b` (see the module).
pub struct Branded<'b, 't, I, T> {
    values: &'t [T],
    _brand: Brand<'b>,
    _id: PhantomData<fn(I) -> I>,
}

impl<I, T> Clone for Branded<'_, '_, I, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<I, T> Copy for Branded<'_, '_, I, T> {}

impl<'b, 't, I: Id, T> Branded<'b, 't, I, T> {
    #[inline(always)]
    pub fn len(self) -> usize {
        self.values.len()
    }

    #[inline(always)]
    pub fn is_empty(self) -> bool {
        self.values.is_empty()
    }

    /// `id`, proven in range for this view; `None` past the end.
    #[inline(always)]
    pub fn check(self, id: I) -> Option<Ix<'b, I>> {
        if id.index() < self.values.len() { Some(Ix { id, _brand: PhantomData }) } else { None }
    }

    /// The entry `ix` names, with no bounds check.
    #[inline(always)]
    pub fn get(self, ix: Ix<'b, I>) -> &'t T {
        // SAFETY: `ix` carries this view's brand, so it was made by `check` or `ids` of this
        // view (the brand is fresh per `Table::checked` and invariant), each of which gives
        // only indices below `values.len()`, and `values` is a shared borrow that cannot
        // change length while the brand lives (the module's argument, points 1 to 3).
        unsafe { self.values.get_unchecked(ix.id.index()) }
    }

    /// Every id of the view, each already proven in range.
    #[inline]
    pub fn ids(self) -> impl ExactSizeIterator<Item = Ix<'b, I>> + DoubleEndedIterator {
        (0..raw_of(self.values.len())).map(|raw| Ix { id: I::from_u32(raw), _brand: PhantomData })
    }

    /// The entries as a slice.
    #[inline(always)]
    pub fn as_slice(self) -> &'t [T] {
        self.values
    }
}

/// A set of the ids of a table: one bit per id, up to the largest inserted.
pub struct TableSet<I: Id, A: Allocator> {
    words: Vec<u64, A>,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, A: Allocator> TableSet<I, A> {
    pub const fn new_in(alloc: A) -> TableSet<I, A> {
        TableSet { words: Vec::new_in(alloc), _id: PhantomData }
    }

    /// An empty set with room for ids below `ids` without growing.
    pub fn try_with_capacity_in(ids: usize, alloc: A) -> Result<TableSet<I, A>, AllocFailure> {
        Ok(TableSet { words: Vec::try_with_capacity_in(ids.div_ceil(64), alloc)?, _id: PhantomData })
    }

    #[inline]
    pub fn contains(&self, id: I) -> bool {
        let at = id.index();
        match self.words.get(at / 64) {
            Some(word) => word >> (at % 64) & 1 != 0,
            None => false,
        }
    }

    /// Adds `id`; whether it was new. Grows to `id`'s word, refused with nothing changed.
    #[inline]
    pub fn try_insert(&mut self, id: I) -> Result<bool, AllocFailure> {
        let at = id.index();
        let word = at / 64;
        if word >= self.words.len() {
            self.words.try_reserve(word + 1 - self.words.len())?;
            self.words.resize(word + 1, 0);
        }
        let bit = 1u64 << (at % 64);
        let slot = &mut self.words[word];
        let new = *slot & bit == 0;
        *slot |= bit;
        Ok(new)
    }

    /// [`TableSet::try_insert`], panicking with the refusal.
    #[inline]
    pub fn insert(&mut self, id: I) -> bool {
        match self.try_insert(id) {
            Ok(new) => new,
            Err(failure) => fail(failure),
        }
    }

    /// Takes `id` out; whether it was in.
    #[inline]
    pub fn remove(&mut self, id: I) -> bool {
        let at = id.index();
        match self.words.get_mut(at / 64) {
            Some(slot) => {
                let bit = 1u64 << (at % 64);
                let was = *slot & bit != 0;
                *slot &= !bit;
                was
            }
            None => false,
        }
    }

    /// Empties the set, keeping its room.
    pub fn clear(&mut self) {
        self.words.clear();
    }

    /// How many ids are in: a popcount of every word.
    pub fn count(&self) -> usize {
        self.words.iter().map(|w| blaze_int::idx(w.count_ones())).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// The ids in, ascending.
    pub fn iter(&self) -> impl Iterator<Item = I> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            let base = at * 64;
            Bits(word).map(move |bit| I::from_u32(raw_of(base + bit)))
        })
    }
}

/// The set bits of a word, lowest first.
struct Bits(u64);

impl Iterator for Bits {
    type Item = usize;

    #[inline]
    fn next(&mut self) -> Option<usize> {
        if self.0 == 0 {
            return None;
        }
        let bit = self.0.trailing_zeros();
        self.0 &= self.0 - 1;
        Some(blaze_int::idx(bit))
    }
}

/// A dense side table from a table's ids to `V`: a slot per id up to the largest inserted.
pub struct TableMap<I: Id, V, A: Allocator> {
    slots: Vec<Option<V>, A>,
    len: usize,
    _id: PhantomData<fn(I) -> I>,
}

impl<I: Id, V, A: Allocator> TableMap<I, V, A> {
    pub const fn new_in(alloc: A) -> TableMap<I, V, A> {
        TableMap { slots: Vec::new_in(alloc), len: 0, _id: PhantomData }
    }

    /// An empty map with room for ids below `ids` without growing.
    pub fn try_with_capacity_in(ids: usize, alloc: A) -> Result<TableMap<I, V, A>, AllocFailure> {
        Ok(TableMap { slots: Vec::try_with_capacity_in(ids, alloc)?, len: 0, _id: PhantomData })
    }

    /// How many ids have a value.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn get(&self, id: I) -> Option<&V> {
        self.slots.get(id.index()).and_then(Option::as_ref)
    }

    #[inline]
    pub fn get_mut(&mut self, id: I) -> Option<&mut V> {
        self.slots.get_mut(id.index()).and_then(Option::as_mut)
    }

    #[inline]
    pub fn contains_key(&self, id: I) -> bool {
        self.get(id).is_some()
    }

    /// Sets `id`'s value, giving back the one before. Grows to `id`'s slot; refused with the
    /// value back and nothing changed.
    #[inline]
    pub fn try_insert(&mut self, id: I, value: V) -> Result<Option<V>, Refused<V>> {
        let at = id.index();
        if at >= self.slots.len() {
            if let Err(why) = self.slots.try_reserve(at + 1 - self.slots.len()) {
                return Err(Refused { value, why });
            }
            self.slots.resize_with(at + 1, || None);
        }
        let before = self.slots[at].replace(value);
        if before.is_none() {
            self.len += 1;
        }
        Ok(before)
    }

    /// [`TableMap::try_insert`], panicking with the refusal.
    #[inline]
    pub fn insert(&mut self, id: I, value: V) -> Option<V> {
        match self.try_insert(id, value) {
            Ok(before) => before,
            Err(refused) => fail(refused.why),
        }
    }

    /// Takes `id`'s value out.
    pub fn remove(&mut self, id: I) -> Option<V> {
        let taken = self.slots.get_mut(id.index()).and_then(Option::take);
        if taken.is_some() {
            self.len -= 1;
        }
        taken
    }

    /// Empties the map, keeping its room.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.len = 0;
    }

    /// Each id with a value, ascending.
    pub fn iter(&self) -> impl Iterator<Item = (I, &V)> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(at, slot)| slot.as_ref().map(|value| (I::from_u32(raw_of(at)), value)))
    }
}
