//! The columns an [`InferenceResult`](super::InferenceResult) is made of
//! (docs/v2/plan/design/data.md): what is known of each expression, pattern or binding sits in
//! a column indexed by its arena index, lists are CSR ranges into one flat array, sets are
//! bitsets, and what few nodes have (closures, opaque types, `asm!` symbols) sits in a sorted
//! side table.
//!
//! Every column is canonical: two results that say the same thing are the same columns,
//! whatever order inference wrote them in, so derived `Eq` and `Hash` agree with what the
//! result means.

use std::{
    fmt,
    marker::PhantomData,
    ops::Index,
};

use blaze_ids::bits::{BitSet, Bits, Ranked};
use blaze_mem::Frame;
use hir_def::{
    FieldId, TupleFieldId,
    hir::{ExprId, ExprOrPatIdPacked, PatId},
};
use la_arena::{Idx, RawIdx};
use rustc_ast_ir::Mutability;

pub(crate) use crate::{FrameMap, FrameSet, frame_map, frame_set};
use crate::{
    infer::{BindingMode, ByRef},
    next_solver::StoredTy,
};

/// No value for this key, in a column of places.
pub(crate) const NONE: u32 = u32::MAX;

/// What a dense column is indexed by: an arena index.
pub trait DenseKey: Copy {
    fn to_index(self) -> usize;
    fn from_index(index: usize) -> Self;
}

impl<T> DenseKey for Idx<T> {
    #[inline]
    fn to_index(self) -> usize {
        self.into_raw().into_u32() as usize
    }

    #[inline]
    fn from_index(index: usize) -> Self {
        Idx::from_raw(RawIdx::from_u32(u32::try_from(index).expect("an arena of 2^32 nodes")))
    }
}

fn place(index: usize) -> u32 {
    let place = u32::try_from(index).expect("a column of 2^32 values");
    assert!(place != NONE, "a column of 2^32 values");
    place
}

/// Which keys of a sparse column have a row, and where it is: a bit per key, ranked, so a
/// row's place is one popcount. Not a bare [`Ranked`]: its last word is never zero, so a column
/// has one form and two that hold the same keys are equal. The ranks are made again from the
/// bits, never stored.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Presence(Ranked);

impl Presence {
    fn new(words: Vec<u64>) -> Result<Presence, &'static str> {
        if words.last() == Some(&0) {
            return Err("a sparse column whose last word of keys has none");
        }
        let len = words.len() * 64;
        Ranked::from_words(words, len).map(Presence).ok_or("a column of 2^32 rows")
    }

    /// Which keys have a row: bit `key % 64` of word `key / 64`.
    fn words(&self) -> &[u64] {
        self.0.words()
    }

    /// How many keys have a row.
    fn count(&self) -> usize {
        self.0.count()
    }

    /// The place of `index`'s row among the rows, if it has one.
    #[inline]
    fn place(&self, index: usize) -> Option<usize> {
        self.0.place(index)
    }

    /// Each key with a row and the row's place, in key order.
    fn keys(&self) -> impl DoubleEndedIterator<Item = (usize, usize)> + '_ {
        self.0.iter_ranked()
    }
}

/// How many of `slots` a column of them keeps: up to the last with a value.
fn used<V>(slots: &[Option<V>]) -> usize {
    slots.iter().rposition(Option::is_some).map_or(0, |last| last + 1)
}

/// Which of `slots` have a value, as a [`Presence`] made at its length.
fn presence_of<V>(slots: &[Option<V>]) -> Presence {
    let used = used(slots);
    // alloc: the column's own words, at their length
    let mut words = BitSet::<usize>::from_words(vec![0u64; used.div_ceil(64)]);
    for (index, slot) in slots[..used].iter().enumerate() {
        if slot.is_some() {
            words.insert(index);
        }
    }
    Presence::new(words.into_words()).expect("the last used slot has a value")
}

/// The values of `entries` by key, in key order, in `frame`.
#[cfg(test)]
fn dense_in<K: DenseKey, V>(
    frame: &Frame,
    entries: impl IntoIterator<Item = (K, V)>,
) -> Vec<Option<V>, &Frame> {
    let mut dense = Vec::new_in(frame);
    for (key, value) in entries {
        let index = key.to_index();
        if index >= dense.len() {
            dense.resize_with(index + 1, || None);
        }
        dense[index] = Some(value);
    }
    dense
}

/// A value per arena index while inference writes it, in the frame: `ArenaMap`'s slots, a
/// `None` per index without a value.
pub(crate) struct DenseMap<'f, K, V> {
    slots: Vec<Option<V>, &'f Frame>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K: DenseKey, V> DenseMap<'f, K, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        DenseMap { slots: Vec::new_in(frame), key: PhantomData }
    }

    /// Room for the keys below `len`, so that writing them moves nothing in the frame.
    pub(crate) fn reserve_keys(&mut self, len: usize) {
        if len > self.slots.len() {
            self.slots.resize_with(len, || None);
        }
    }

    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let index = key.to_index();
        if index >= self.slots.len() {
            self.slots.resize_with(index + 1, || None);
        }
        self.slots[index].replace(value)
    }

    #[inline]
    pub(crate) fn get(&self, key: K) -> Option<&V> {
        self.slots.get(key.to_index())?.as_ref()
    }

    pub(crate) fn contains_idx(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().flatten()
    }

    /// The slots, a `None` for each key without a value.
    pub(crate) fn slots(&self) -> &[Option<V>] {
        &self.slots
    }

    pub(crate) fn slots_mut(&mut self) -> &mut [Option<V>] {
        &mut self.slots
    }

    #[allow(dead_code, reason = "ArenaMap's API, which DenseMap replaces for inference")]
    pub(crate) fn get_mut(&mut self, key: K) -> Option<&mut V> {
        self.slots.get_mut(key.to_index())?.as_mut()
    }

    /// The keys with a value and their values, in key order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (K, &V)> {
        (self.slots.iter().enumerate())
            .filter_map(|(index, slot)| Some((K::from_index(index), slot.as_ref()?)))
    }

    /// The keys with a value and their values, taken out in key order: `ArenaMap::into_iter`.
    #[allow(dead_code, reason = "ArenaMap's API, which DenseMap replaces for inference")]
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = (K, V)> {
        (self.slots.drain(..).enumerate())
            .filter_map(|(index, slot)| Some((K::from_index(index), slot?)))
    }
}

impl<'f, K: DenseKey, V: Clone> DenseMap<'f, K, V> {
    #[cfg(test)]
    fn clone_for_test(&self) -> Self {
        DenseMap { slots: self.slots.clone(), key: PhantomData }
    }
}

impl<K: DenseKey, V> Index<K> for DenseMap<'_, K, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, key: K) -> &V {
        self.get(key).expect("no value for this key")
    }
}

impl<K: DenseKey + fmt::Debug, V: fmt::Debug> fmt::Debug for DenseMap<'_, K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A type per key, dictionary-encoded: `at[key]` is where the key's type is in `tys`
/// ([`NONE`] for a key without one), each distinct type in `tys` once, in the order of the
/// first key that has it.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TyColumn<K> {
    at: Box<[u32]>,
    tys: Box<[StoredTy]>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for TyColumn<K> {
    fn default() -> Self {
        TyColumn { at: Box::default(), tys: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey> TyColumn<K> {
    /// The column of `slots`, which it takes the types out of.
    pub(crate) fn from_dense(frame: &Frame, slots: &mut [Option<StoredTy>]) -> Self {
        let used = used(slots);
        let mut by_ty: FrameMap<'_, StoredTy, u32> = frame_map(frame);
        let mut tys = Vec::new_in(frame);
        // alloc: the column's own places, at their length
        let mut at = Vec::with_capacity(used);
        for slot in &mut slots[..used] {
            at.push(match slot.take() {
                None => NONE,
                Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                    tys.push(*ty);
                    place(tys.len() - 1)
                }),
            });
        }
        // alloc: the column's own dictionary, copied out of the frame at its length
        TyColumn { at: at.into_boxed_slice(), tys: tys.drain(..).collect(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, StoredTy)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            Self::from_dense(frame, &mut dense_in(frame, entries))
        })
    }

    /// The column of `at` and `tys` as [`Self::at`] and [`Self::tys`] give them, refused when
    /// it is not one [`Self::from_dense`] makes.
    pub(crate) fn from_parts(at: Box<[u32]>, tys: Box<[StoredTy]>) -> Result<Self, &'static str> {
        if at.last() == Some(&NONE) {
            return Err("a type column that ends in a key without a type");
        }
        let mut next = 0u32;
        for &place in &at {
            match place {
                NONE => {}
                _ if place < next => {}
                _ if place == next && (place as usize) < tys.len() => next += 1,
                _ => return Err("a type column out of its dictionary's order"),
            }
        }
        if next as usize != tys.len() {
            return Err("a type in a column's dictionary that no key has");
        }
        Ok(TyColumn { at, tys, key: PhantomData })
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&StoredTy> {
        match self.at.get(key.to_index()) {
            Some(&place) if place != NONE => Some(&self.tys[place as usize]),
            _ => None,
        }
    }

    pub fn contains_idx(&self, key: K) -> bool {
        self.at.get(key.to_index()).is_some_and(|&place| place != NONE)
    }

    /// Each key with a type, in key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &StoredTy)> + '_ {
        (self.at.iter().enumerate())
            .filter(|&(_, &place)| place != NONE)
            .map(|(index, &place)| (K::from_index(index), &self.tys[place as usize]))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &StoredTy> + '_ {
        self.iter().map(|(_, ty)| ty)
    }

    pub fn is_empty(&self) -> bool {
        self.at.iter().all(|&place| place == NONE)
    }

    /// Where each key's type is in [`Self::tys`], [`NONE`] for a key without one.
    pub fn at(&self) -> &[u32] {
        &self.at
    }

    /// Each distinct type once.
    pub fn tys(&self) -> &[StoredTy] {
        &self.tys
    }
}

impl<K: DenseKey> Index<K> for TyColumn<K> {
    type Output = StoredTy;

    #[track_caller]
    fn index(&self, key: K) -> &StoredTy {
        self.get(key).expect("no type was inferred for this node")
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for TyColumn<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A row per key with one, for columns most keys have no row in: which keys have one as a
/// [`Presence`], the rows in key order.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IndexColumn<K, V> {
    present: Presence,
    rows: Box<[V]>,
    key: PhantomData<fn() -> K>,
}

impl<K, V> Default for IndexColumn<K, V> {
    fn default() -> Self {
        IndexColumn { present: Presence::default(), rows: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey, V> IndexColumn<K, V> {
    /// The column of `slots`, which it takes the rows out of.
    pub(crate) fn from_dense(slots: &mut [Option<V>]) -> Self {
        let present = presence_of(slots);
        // alloc: the column's own rows, at their length
        let mut rows = Vec::with_capacity(present.count());
        rows.extend(slots.iter_mut().filter_map(Option::take));
        IndexColumn { present, rows: rows.into_boxed_slice(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
            Self::from_dense(&mut dense_in(frame, entries))
        })
    }

    /// The column of `present` and `rows` as [`Self::present`] and [`Self::rows`] give them,
    /// refused when it is not one [`Self::from_dense`] makes.
    pub(crate) fn from_parts(present: Vec<u64>, rows: Box<[V]>) -> Result<Self, &'static str> {
        let present = Presence::new(present)?;
        if present.count() != rows.len() {
            return Err("a row column whose keys and rows differ in number");
        }
        Ok(IndexColumn { present, rows, key: PhantomData })
    }

    /// Which keys have a row: bit `key % 64` of word `key / 64`, the last word never zero.
    pub fn present(&self) -> &[u64] {
        self.present.words()
    }

    /// The rows, in key order.
    pub fn rows(&self) -> &[V] {
        &self.rows
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&V> {
        self.present.place(key.to_index()).map(|place| &self.rows[place])
    }

    pub fn contains_idx(&self, key: K) -> bool {
        self.present.place(key.to_index()).is_some()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &V)> + '_ {
        self.present.keys().map(|(index, place)| (K::from_index(index), &self.rows[place]))
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<K: DenseKey + fmt::Debug, V: fmt::Debug> fmt::Debug for IndexColumn<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// The same column of expressions and of patterns, keyed by either.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ExprOrPatColumn<V> {
    pub(crate) exprs: IndexColumn<ExprId, V>,
    pub(crate) pats: IndexColumn<PatId, V>,
}

impl<V> Default for ExprOrPatColumn<V> {
    fn default() -> Self {
        ExprOrPatColumn { exprs: IndexColumn::default(), pats: IndexColumn::default() }
    }
}

impl<V> ExprOrPatColumn<V> {
    #[inline]
    pub fn get(&self, node: ExprOrPatIdPacked) -> Option<&V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.get(expr),
            (None, Some(pat)) => self.pats.get(pat),
            (None, None) => None,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (ExprOrPatIdPacked, &V)> + '_ {
        let exprs = self.exprs.iter().map(|(expr, value)| (ExprOrPatIdPacked::from(expr), value));
        exprs.chain(self.pats.iter().map(|(pat, value)| (ExprOrPatIdPacked::from(pat), value)))
    }
}

impl<V: fmt::Debug> fmt::Debug for ExprOrPatColumn<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A map of expressions or patterns to values while inference writes it, in the frame.
#[derive(Debug)]
pub(crate) struct ExprOrPatMap<'f, V> {
    exprs: DenseMap<'f, ExprId, V>,
    pats: DenseMap<'f, PatId, V>,
}

impl<'f, V> ExprOrPatMap<'f, V> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        ExprOrPatMap { exprs: DenseMap::new_in(frame), pats: DenseMap::new_in(frame) }
    }

    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked, value: V) -> Option<V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr, value),
            (None, Some(pat)) => self.pats.insert(pat, value),
            (None, None) => None,
        }
    }

    pub(crate) fn get(&self, node: ExprOrPatIdPacked) -> Option<&V> {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.get(expr),
            (None, Some(pat)) => self.pats.get(pat),
            (None, None) => None,
        }
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.exprs.values_mut().chain(self.pats.values_mut())
    }

    pub(crate) fn freeze(mut self) -> ExprOrPatColumn<V> {
        ExprOrPatColumn {
            exprs: IndexColumn::from_dense(self.exprs.slots_mut()),
            pats: IndexColumn::from_dense(self.pats.slots_mut()),
        }
    }
}

/// What a field access resolves to.
const FIELD_NONE: u8 = 0;
const FIELD: u8 = 1;
const TUPLE_FIELD: u8 = 2;

/// The field each field access expression resolves to: a tag per expression ([`FIELD_NONE`],
/// [`FIELD`] or [`TUPLE_FIELD`]) and a word, its place in `fields` or `tuple_fields`.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct FieldColumn {
    tags: Box<[u8]>,
    data: Box<[u32]>,
    fields: Box<[FieldId]>,
    tuple_fields: Box<[TupleFieldId]>,
}

impl FieldColumn {
    pub(crate) fn from_dense(slots: &[Option<either::Either<FieldId, TupleFieldId>>]) -> Self {
        let slots = &slots[..used(slots)];
        let tuple_count = slots.iter().filter(|it| matches!(it, Some(either::Either::Right(_)))).count();
        let field_count = slots.iter().filter(|it| it.is_some()).count() - tuple_count;
        // alloc: the column's own parts, each at its length
        let (mut fields, mut tuple_fields) =
            (Vec::with_capacity(field_count), Vec::with_capacity(tuple_count));
        let mut tags = Vec::with_capacity(slots.len());
        let mut data = Vec::with_capacity(slots.len());
        for &field in slots {
            let (tag, word) = match field {
                None => (FIELD_NONE, 0),
                Some(either::Either::Left(field)) => {
                    fields.push(field);
                    (FIELD, place(fields.len() - 1))
                }
                Some(either::Either::Right(field)) => {
                    tuple_fields.push(field);
                    (TUPLE_FIELD, place(tuple_fields.len() - 1))
                }
            };
            tags.push(tag);
            data.push(word);
        }
        FieldColumn {
            tags: tags.into_boxed_slice(),
            data: data.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            tuple_fields: tuple_fields.into_boxed_slice(),
        }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(
        entries: impl IntoIterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)>,
    ) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_dense(&dense_in(frame, entries)))
    }

    /// The column of the parts [`Self::tags`], [`Self::data`], [`Self::fields`] and
    /// [`Self::tuple_fields`] give, refused when it is not one [`Self::from_dense`] makes.
    pub(crate) fn from_parts(
        tags: Box<[u8]>,
        data: Box<[u32]>,
        fields: Box<[FieldId]>,
        tuple_fields: Box<[TupleFieldId]>,
    ) -> Result<Self, &'static str> {
        if tags.len() != data.len() {
            return Err("a field column whose tags and words differ in length");
        }
        if tags.last() == Some(&FIELD_NONE) {
            return Err("a field column that ends in an expression without a field");
        }
        let (mut next_field, mut next_tuple_field) = (0u32, 0u32);
        for (&tag, &word) in tags.iter().zip(&data) {
            let next = match tag {
                FIELD_NONE if word == 0 => continue,
                FIELD => &mut next_field,
                TUPLE_FIELD => &mut next_tuple_field,
                _ => return Err("a field column entry that is none of its kinds"),
            };
            if word != *next {
                return Err("a field column out of expression order");
            }
            *next += 1;
        }
        if next_field as usize != fields.len() || next_tuple_field as usize != tuple_fields.len() {
            return Err("a field column with a field no expression has");
        }
        Ok(FieldColumn { tags, data, fields, tuple_fields })
    }

    /// Per expression, none, a field or a tuple field.
    pub fn tags(&self) -> &[u8] {
        &self.tags
    }

    /// Per expression, its field's place in [`Self::fields`] or [`Self::tuple_fields`].
    pub fn data(&self) -> &[u32] {
        &self.data
    }

    pub fn fields(&self) -> &[FieldId] {
        &self.fields
    }

    pub fn tuple_fields(&self) -> &[TupleFieldId] {
        &self.tuple_fields
    }

    #[inline]
    pub fn get(&self, expr: ExprId) -> Option<either::Either<FieldId, TupleFieldId>> {
        let index = expr.to_index();
        match self.tags.get(index).copied()? {
            FIELD => Some(either::Either::Left(self.fields[self.data[index] as usize])),
            TUPLE_FIELD => Some(either::Either::Right(self.tuple_fields[self.data[index] as usize])),
            _ => None,
        }
    }

    pub fn iter(
        &self,
    ) -> impl Iterator<Item = (ExprId, either::Either<FieldId, TupleFieldId>)> + '_ {
        (0..self.tags.len())
            .filter_map(|index| Some((ExprId::from_index(index), self.get(ExprId::from_index(index))?)))
    }
}

impl fmt::Debug for FieldColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// Where a key's list is in a CSR column's flat array: [`NONE`] as `start` for a key without
/// one, which is not the same as an empty list.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Range {
    pub start: u32,
    pub len: u32,
}

impl Range {
    pub const ABSENT: Range = Range { start: NONE, len: 0 };

    #[inline]
    fn is_absent(self) -> bool {
        self.start == NONE
    }

    #[inline]
    fn slice(self) -> std::ops::Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }
}

const _: () = assert!(size_of::<Range>() == 8);

/// A list per key as CSR, for columns most keys have no list in: which keys have one (an
/// empty list is not none) as a [`Presence`], and where each list ends in one flat array, the
/// lists in key order.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Csr<K, T> {
    present: Presence,
    ends: Box<[u32]>,
    items: Box<[T]>,
    key: PhantomData<fn() -> K>,
}

impl<K, T> Default for Csr<K, T> {
    fn default() -> Self {
        Csr { present: Presence::default(), ends: Box::default(), items: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey, T> Csr<K, T> {
    /// The column of `present`, `ends` and `items` as [`Self::present`], [`Self::ends`] and
    /// [`Self::items`] give them, refused when it is not one [`CsrBuilder::freeze`] makes.
    pub(crate) fn from_parts(
        present: Vec<u64>,
        ends: Box<[u32]>,
        items: Box<[T]>,
    ) -> Result<Self, &'static str> {
        let present = Presence::new(present)?;
        if present.count() != ends.len() {
            return Err("a list column whose keys and lists differ in number");
        }
        if !ends.is_sorted() {
            return Err("a list column whose lists end out of order");
        }
        if ends.last().map_or(0, |&end| end as usize) != items.len() {
            return Err("a list column whose lists do not cover its items");
        }
        Ok(Csr { present, ends, items, key: PhantomData })
    }

    #[inline]
    fn list(&self, place: usize) -> &[T] {
        let start = match place {
            0 => 0,
            _ => self.ends[place - 1] as usize,
        };
        &self.items[start..self.ends[place] as usize]
    }

    #[inline]
    pub fn get(&self, key: &K) -> Option<&[T]> {
        self.present.place(key.to_index()).map(|place| self.list(place))
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.present.place(key.to_index()).is_some()
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (K, &[T])> + '_ {
        self.present.keys().map(|(index, place)| (K::from_index(index), self.list(place)))
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &[T]> + '_ {
        self.iter().map(|(_, items)| items)
    }

    pub fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Which keys have a list: bit `key % 64` of word `key / 64`, the last word never zero.
    pub fn present(&self) -> &[u64] {
        self.present.words()
    }

    /// Where each list ends in [`Self::items`], in key order.
    pub fn ends(&self) -> &[u32] {
        &self.ends
    }

    /// Every key's list, one after another.
    pub fn items(&self) -> &[T] {
        &self.items
    }
}

impl<K: DenseKey + fmt::Debug, T: fmt::Debug> fmt::Debug for Csr<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A CSR column while inference writes it: a list that is replaced or grows past the end of
/// the flat array moves to its end, leaving its old items unused until [`Self::freeze`]
/// compacts the lists into key order.
#[derive(Clone, Debug)]
pub(crate) struct CsrBuilder<'f, K, T> {
    ranges: Vec<Range, &'f Frame>,
    items: Vec<T, &'f Frame>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K, T> CsrBuilder<'f, K, T> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        CsrBuilder { ranges: Vec::new_in(frame), items: Vec::new_in(frame), key: PhantomData }
    }
}

impl<'f, K: DenseKey, T> CsrBuilder<'f, K, T> {
    /// Room for the keys below `len`, so that writing them moves nothing in the frame.
    pub(crate) fn reserve_keys(&mut self, len: usize) {
        if len > self.ranges.len() {
            self.ranges.resize(len, Range::ABSENT);
        }
    }

    fn range(&self, key: &K) -> Range {
        self.ranges.get(key.to_index()).copied().unwrap_or(Range::ABSENT)
    }

    fn set(&mut self, key: &K, range: Range) {
        let index = key.to_index();
        if index >= self.ranges.len() {
            self.ranges.resize(index + 1, Range::ABSENT);
        }
        self.ranges[index] = range;
    }

    fn start(&self) -> u32 {
        place(self.items.len())
    }

    pub(crate) fn get(&self, key: &K) -> Option<&[T]> {
        let range = self.range(key);
        (!range.is_absent()).then(|| &self.items[range.slice()])
    }

    pub(crate) fn get_mut(&mut self, key: &K) -> Option<&mut [T]> {
        let range = self.range(key);
        (!range.is_absent()).then(|| &mut self.items[range.slice()])
    }

    pub(crate) fn contains_key(&self, key: &K) -> bool {
        !self.range(key).is_absent()
    }

    /// How many items `key`'s list has, 0 for a key without one.
    pub(crate) fn len_of(&self, key: &K) -> usize {
        self.range(key).len as usize
    }

    /// Gives `key` an empty list if it has none.
    pub(crate) fn ensure(&mut self, key: K) {
        if !self.contains_key(&key) {
            let start = self.start();
            self.set(&key, Range { start, len: 0 });
        }
    }

    /// Replaces `key`'s list.
    pub(crate) fn insert(&mut self, key: K, items: impl IntoIterator<Item = T>) {
        let start = self.start();
        self.items.extend(items);
        let len = place(self.items.len() - start as usize);
        self.set(&key, Range { start, len });
    }

    /// Leaves `key` without a list, its items unused until [`Self::freeze`]; returns whether it
    /// had one.
    pub(crate) fn remove_in_place(&mut self, key: &K) -> bool {
        if self.range(key).is_absent() {
            return false;
        }
        self.set(key, Range::ABSENT);
        true
    }

    /// Appends `item` to `key`'s list, making one if it has none.
    pub(crate) fn push(&mut self, key: K, item: T)
    where
        T: Clone,
    {
        let mut range = self.range(&key);
        if range.is_absent() {
            range = Range { start: self.start(), len: 0 };
        } else if range.start as usize + range.len as usize != self.items.len() {
            let start = self.start();
            self.items.extend_from_within(range.slice());
            range.start = start;
        }
        self.items.push(item);
        range.len += 1;
        self.set(&key, range);
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut [T]> {
        let CsrBuilder { ranges, items, .. } = self;
        let mut live = Vec::new_in(*ranges.allocator());
        // An empty list may start inside another one; lists with items never overlap, so their
        // starts are distinct and an unstable sort orders them as a stable one does.
        live.extend(ranges.iter().copied().filter(|it| !it.is_absent() && it.len != 0));
        live.sort_unstable_by_key(|range| range.start);
        let mut rest: &mut [T] = items;
        let mut consumed = 0usize;
        live.into_iter().map(move |range| {
            let taken = std::mem::take(&mut rest);
            let (_, tail) = taken.split_at_mut(range.start as usize - consumed);
            let (run, tail) = tail.split_at_mut(range.len as usize);
            consumed = range.start as usize + range.len as usize;
            rest = tail;
            run
        })
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (K, &[T])> + '_ {
        (self.ranges.iter().enumerate())
            .filter(|(_, range)| !range.is_absent())
            .map(|(index, range)| (K::from_index(index), &self.items[range.slice()]))
    }

    /// O(keys) when inference wrote the lists in key order without replacing one (the flat
    /// array is copied out of the frame as it is), else one move of every live item.
    pub(crate) fn freeze(self) -> Csr<K, T> {
        let CsrBuilder { ranges, mut items, .. } = self;
        let frame = *items.allocator();
        let used = ranges.iter().rposition(|range| !range.is_absent()).map_or(0, |last| last + 1);
        let ranges = &ranges[..used];
        let lists = ranges.iter().filter(|range| !range.is_absent()).count();
        // alloc: the column's own words and ends, at their lengths
        let mut words = BitSet::<usize>::from_words(vec![0u64; used.div_ceil(64)]);
        let mut ends = Vec::with_capacity(lists);
        let mut next = 0usize;
        let mut in_order = true;
        for (index, range) in ranges.iter().enumerate() {
            if range.is_absent() {
                continue;
            }
            words.insert(index);
            in_order &= range.start as usize == next;
            next += range.len as usize;
            ends.push(place(next));
        }
        let present = Presence::new(words.into_words()).expect("the last used range is a list");
        let ends = ends.into_boxed_slice();
        if in_order && next == items.len() {
            // alloc: the column's own items, copied out of the frame at their length
            return Csr { present, ends, items: items.drain(..).collect(), key: PhantomData };
        }
        let mut slots = Vec::with_capacity_in(items.len(), frame);
        slots.extend(items.drain(..).map(Some));
        // alloc: the column's own items, at their length
        let mut flat = Vec::with_capacity(next);
        for range in ranges.iter().filter(|range| !range.is_absent()) {
            flat.extend(slots[range.slice()].iter_mut().map(|it: &mut Option<T>| it.take().unwrap()));
        }
        Csr { present, ends, items: flat.into_boxed_slice(), key: PhantomData }
    }
}

/// A set of keys as a bitset, its last word never zero, so two that hold the same keys are
/// equal.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdxSet<K> {
    words: BitSet,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for IdxSet<K> {
    fn default() -> Self {
        IdxSet { words: BitSet::default(), key: PhantomData }
    }
}

impl<K: DenseKey> IdxSet<K> {
    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        self.words.insert_growing(key.to_index())
    }

    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        self.words.contains(key.to_index())
    }

    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.words.iter().map(K::from_index)
    }

    pub fn len(&self) -> usize {
        self.words.count()
    }

    pub fn is_empty(&self) -> bool {
        self.words.words().is_empty()
    }

    pub fn words(&self) -> &[u64] {
        self.words.words()
    }

    pub(crate) fn from_words(words: Vec<u64>) -> Result<Self, &'static str> {
        match words.last() {
            Some(0) => Err("a set whose last word is zero"),
            _ => Ok(IdxSet { words: BitSet::from_words(words), key: PhantomData }),
        }
    }
}

impl<K: DenseKey> FromIterator<K> for IdxSet<K> {
    fn from_iter<I: IntoIterator<Item = K>>(iter: I) -> Self {
        let mut set = IdxSet::default();
        iter.into_iter().for_each(|key| _ = set.insert(key));
        set
    }
}

impl<K: DenseKey> Extend<K> for IdxSet<K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        iter.into_iter().for_each(|key| _ = self.insert(key));
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for IdxSet<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.iter()).finish()
    }
}

/// A set of expressions and patterns.
#[derive(Clone, Default, PartialEq, Eq, Hash, Debug)]
pub struct ExprOrPatSet {
    pub(crate) exprs: IdxSet<ExprId>,
    pub(crate) pats: IdxSet<PatId>,
}

impl ExprOrPatSet {
    #[cfg(test)]
    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr),
            (None, Some(pat)) => self.pats.insert(pat),
            (None, None) => false,
        }
    }

    pub fn contains(&self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.contains(&expr),
            (None, Some(pat)) => self.pats.contains(&pat),
            (None, None) => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.pats.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ExprOrPatIdPacked> + '_ {
        let exprs = self.exprs.iter().map(ExprOrPatIdPacked::from);
        exprs.chain(self.pats.iter().map(ExprOrPatIdPacked::from))
    }
}

/// An [`IdxSet`] while inference writes it, its words in the frame and made at the key count.
#[derive(Debug)]
pub(crate) struct FrameIdxSet<'f, K> {
    words: Bits<Vec<u64, &'f Frame>>,
    key: PhantomData<fn() -> K>,
}

impl<'f, K: DenseKey> FrameIdxSet<'f, K> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        FrameIdxSet { words: Bits::from_words(Vec::new_in(frame)), key: PhantomData }
    }

    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        self.words.insert_growing(key.to_index())
    }

    #[inline]
    pub(crate) fn contains(&self, key: &K) -> bool {
        self.words.contains(key.to_index())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// The set, its words copied once at their length, the last never zero.
    pub(crate) fn freeze(&self) -> IdxSet<K> {
        let words = self.words.words();
        let used = words.iter().rposition(|&word| word != 0).map_or(0, |last| last + 1);
        // alloc: the set's own words, at their length
        IdxSet { words: BitSet::from_words(words[..used].to_vec()), key: PhantomData }
    }

    #[cfg(test)]
    fn reserve_keys_for_test(&mut self, len: usize) {
        self.words.grow(len);
    }
}

impl<K: DenseKey> Extend<K> for FrameIdxSet<'_, K> {
    fn extend<I: IntoIterator<Item = K>>(&mut self, iter: I) {
        iter.into_iter().for_each(|key| _ = self.insert(key));
    }
}

/// An [`ExprOrPatSet`] while inference writes it, in the frame.
#[derive(Debug)]
pub(crate) struct FrameExprOrPatSet<'f> {
    exprs: FrameIdxSet<'f, ExprId>,
    pats: FrameIdxSet<'f, PatId>,
}

impl<'f> FrameExprOrPatSet<'f> {
    pub(crate) fn new_in(frame: &'f Frame) -> Self {
        FrameExprOrPatSet { exprs: FrameIdxSet::new_in(frame), pats: FrameIdxSet::new_in(frame) }
    }

    pub(crate) fn insert(&mut self, node: ExprOrPatIdPacked) -> bool {
        match (node.as_expr(), node.as_pat()) {
            (Some(expr), _) => self.exprs.insert(expr),
            (None, Some(pat)) => self.pats.insert(pat),
            (None, None) => false,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.exprs.is_empty() && self.pats.is_empty()
    }

    pub(crate) fn freeze(&self) -> ExprOrPatSet {
        ExprOrPatSet { exprs: self.exprs.freeze(), pats: self.pats.freeze() }
    }
}

/// No binding mode, in a [`ModeColumn`].
const MODE_NONE: u8 = u8::MAX;

/// Every binding mode, by its code: `2 * by_ref + mutability`, `by_ref` being 0 for by value,
/// 1 for `ref` and 2 for `ref mut`.
static BINDING_MODES: [BindingMode; 6] = [
    BindingMode(ByRef::No, Mutability::Not),
    BindingMode(ByRef::No, Mutability::Mut),
    BindingMode(ByRef::Yes(Mutability::Not), Mutability::Not),
    BindingMode(ByRef::Yes(Mutability::Not), Mutability::Mut),
    BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Not),
    BindingMode(ByRef::Yes(Mutability::Mut), Mutability::Mut),
];

fn mode_code(mode: BindingMode) -> u8 {
    let BindingMode(by_ref, mutability) = mode;
    let by_ref = match by_ref {
        ByRef::No => 0,
        ByRef::Yes(Mutability::Not) => 1,
        ByRef::Yes(Mutability::Mut) => 2,
    };
    let mutability = match mutability {
        Mutability::Not => 0,
        Mutability::Mut => 1,
    };
    2 * by_ref + mutability
}

/// A binding mode per key, one byte each.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct ModeColumn<K> {
    codes: Box<[u8]>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for ModeColumn<K> {
    fn default() -> Self {
        ModeColumn { codes: Box::default(), key: PhantomData }
    }
}

impl<K: DenseKey> ModeColumn<K> {
    pub(crate) fn from_dense(slots: &[Option<BindingMode>]) -> Self {
        let codes = slots[..used(slots)].iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
        // alloc: the column's own codes, at their length
        ModeColumn { codes: codes.collect(), key: PhantomData }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, BindingMode)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_dense(&dense_in(frame, entries)))
    }

    pub(crate) fn from_codes(codes: Box<[u8]>) -> Result<Self, &'static str> {
        if codes.last() == Some(&MODE_NONE) {
            return Err("a binding mode column that ends in a pattern without one");
        }
        match codes.iter().all(|&code| code == MODE_NONE || (code as usize) < BINDING_MODES.len()) {
            true => Ok(ModeColumn { codes, key: PhantomData }),
            false => Err("a binding mode that is none of them"),
        }
    }

    #[inline]
    pub fn get(&self, key: K) -> Option<&BindingMode> {
        BINDING_MODES.get(*self.codes.get(key.to_index())? as usize)
    }

    pub fn iter(&self) -> impl Iterator<Item = (K, &BindingMode)> + '_ {
        (0..self.codes.len()).filter_map(|index| Some((K::from_index(index), self.get(K::from_index(index))?)))
    }

    pub fn codes(&self) -> &[u8] {
        &self.codes
    }
}

impl<K: DenseKey> Index<K> for ModeColumn<K> {
    type Output = BindingMode;

    #[track_caller]
    fn index(&self, key: K) -> &BindingMode {
        self.get(key).expect("no binding mode was inferred for this pattern")
    }
}

impl<K: DenseKey + fmt::Debug> fmt::Debug for ModeColumn<K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A side table of the few keys that have a value, sorted by key.
#[derive(Clone, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct SortedMap<K, V> {
    entries: Box<[(K, V)]>,
}

impl<K, V> Default for SortedMap<K, V> {
    fn default() -> Self {
        SortedMap { entries: Box::default() }
    }
}

impl<K: Ord, V> SortedMap<K, V> {
    /// The map of `entries`, of distinct keys, sorted in `frame`.
    pub(crate) fn from_entries_in(frame: &Frame, entries: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut sorted = Vec::new_in(frame);
        sorted.extend(entries);
        // The keys are distinct, so an unstable sort orders them as a stable one does.
        sorted.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
        debug_assert!(sorted.windows(2).all(|it| it[0].0 != it[1].0), "a key twice in a map");
        // alloc: the map's own entries, at their length
        SortedMap { entries: sorted.drain(..).collect() }
    }

    #[cfg(test)]
    pub(crate) fn from_entries(entries: impl IntoIterator<Item = (K, V)>) -> Self {
        blaze_mem::frame::task(crate::db::INFER_TAG, |frame| Self::from_entries_in(frame, entries))
    }

    pub(crate) fn from_sorted(entries: Vec<(K, V)>) -> Result<Self, &'static str> {
        match entries.windows(2).all(|it| it[0].0 < it[1].0) {
            true => Ok(SortedMap { entries: entries.into_boxed_slice() }),
            false => Err("a side table out of key order"),
        }
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        let at = self.entries.binary_search_by(|(it, _)| it.cmp(key)).ok()?;
        Some(&self.entries[at].1)
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.get(key).is_some()
    }

    pub fn iter(&self) -> SortedMapIter<'_, K, V> {
        SortedMapIter { entries: self.entries.iter() }
    }

    pub fn keys(&self) -> impl ExactSizeIterator<Item = &K> + '_ {
        self.entries.iter().map(|(key, _)| key)
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = &V> + '_ {
        self.entries.iter().map(|(_, value)| value)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl<K: Ord, V> Index<&K> for SortedMap<K, V> {
    type Output = V;

    #[track_caller]
    fn index(&self, key: &K) -> &V {
        self.get(key).expect("no such key in the side table")
    }
}

pub struct SortedMapIter<'a, K, V> {
    entries: std::slice::Iter<'a, (K, V)>,
}

impl<'a, K, V> Iterator for SortedMapIter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<Self::Item> {
        self.entries.next().map(|(key, value)| (key, value))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.entries.size_hint()
    }
}

impl<K, V> ExactSizeIterator for SortedMapIter<'_, K, V> {}

impl<K, V> DoubleEndedIterator for SortedMapIter<'_, K, V> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.entries.next_back().map(|(key, value)| (key, value))
    }
}

impl<'a, K: Ord, V> IntoIterator for &'a SortedMap<K, V> {
    type Item = (&'a K, &'a V);
    type IntoIter = SortedMapIter<'a, K, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for SortedMap<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(key, value)| (key, value))).finish()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use blaze_ids::rng::SplitMix64;
    use blaze_mem::frame;
    use la_arena::{ArenaMap, Idx, RawIdx};

    use super::*;

    struct Node;
    type Key = Idx<Node>;

    fn key(index: u32) -> Key {
        Idx::from_raw(RawIdx::from_u32(index))
    }

    /// A deterministic sequence of `(key, value)` writes, with overwrites.
    fn writes(seed: u64, count: usize, keys: u32, values: u32) -> Vec<(u32, u32)> {
        let mut rng = SplitMix64::new(seed);
        (0..count)
            .map(|_| (rng.below(u64::from(keys)) as u32, rng.below(u64::from(values)) as u32))
            .collect()
    }

    #[test]
    fn index_column_agrees_with_a_map() {
        for seed in 0..32 {
            let mut model = BTreeMap::new();
            let mut arena = ArenaMap::default();
            for (k, v) in writes(seed, 200, 300, 1000) {
                model.insert(k, v);
                arena.insert(key(k), v);
            }
            let column = IndexColumn::from_entries(arena);
            for k in 0..400 {
                assert_eq!(column.get(key(k)), model.get(&k));
            }
            let listed: Vec<(u32, u32)> =
                column.iter().map(|(k, v)| (k.into_raw().into_u32(), *v)).collect();
            assert_eq!(listed, model.into_iter().collect::<Vec<_>>());
        }
    }

    #[test]
    fn ty_column_agrees_with_a_map_and_keeps_one_of_each_type() {
        let types = &crate::next_solver::default_types().types;
        let palette = [types.i32, types.u8, types.bool, types.unit, types.never];
        for seed in 0..32 {
            let mut model = BTreeMap::new();
            let mut arena = ArenaMap::default();
            for (k, v) in writes(seed, 150, 200, palette.len() as u32) {
                let ty = palette[v as usize].store();
                model.insert(k, ty.clone());
                arena.insert(key(k), ty);
            }
            let column = TyColumn::from_entries(arena.clone());
            for k in 0..250 {
                assert_eq!(column.get(key(k)), model.get(&k));
                assert_eq!(column.contains_idx(key(k)), model.contains_key(&k));
            }
            let distinct: std::collections::HashSet<_> = model.values().cloned().collect();
            assert_eq!(column.tys().len(), distinct.len());
            let listed: Vec<(u32, StoredTy)> =
                column.iter().map(|(k, ty)| (k.into_raw().into_u32(), ty.clone())).collect();
            assert_eq!(listed, model.into_iter().collect::<Vec<_>>());

            let reversed: Vec<(Key, StoredTy)> = arena.iter().rev().map(|(k, ty)| (k, ty.clone())).collect();
            assert!(TyColumn::from_entries(reversed) == column);
            let again = TyColumn::<Key>::from_parts(column.at().into(), column.tys().into()).unwrap();
            assert!(again == column);
        }
        let swapped = TyColumn::<Key>::from_parts(
            Box::new([1, 0]),
            Box::new([types.i32.store(), types.u8.store()]),
        );
        assert!(swapped.is_err());
    }

    #[test]
    fn csr_builder_agrees_with_a_map_of_lists() {
        frame::task(crate::db::INFER_TAG, |frame| {
            for seed in 0..32 {
                let mut model: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
                let mut builder = CsrBuilder::<Key, u32>::new_in(frame);
                for (step, (k, v)) in writes(seed, 400, 40, 1000).into_iter().enumerate() {
                    match step % 5 {
                        0 => {
                            let items = vec![v, v + 1, v + 2];
                            model.insert(k, items.clone());
                            builder.insert(key(k), items);
                        }
                        1 => {
                            let removed = builder.get(&key(k)).map(|it| it.to_vec());
                            assert_eq!(removed, model.remove(&k));
                            assert_eq!(builder.remove_in_place(&key(k)), removed.is_some());
                        }
                        2 => {
                            model.entry(k).or_default();
                            builder.ensure(key(k));
                        }
                        3 => {
                            if let Some(items) = model.get_mut(&k) {
                                items.iter_mut().for_each(|it| *it += 1);
                            }
                            if let Some(items) = builder.get_mut(&key(k)) {
                                items.iter_mut().for_each(|it| *it += 1);
                            }
                        }
                        _ => {
                            model.entry(k).or_default().push(v);
                            builder.push(key(k), v);
                        }
                    }
                    for k in 0..40 {
                        assert_eq!(builder.get(&key(k)).map(|it| it.to_vec()), model.get(&k).cloned());
                        assert_eq!(builder.len_of(&key(k)), model.get(&k).map_or(0, Vec::len));
                    }
                }
                let mut doubled = builder.clone();
                doubled.values_mut().flatten().for_each(|it| *it *= 2);
                let frozen = builder.freeze();
                let listed: Vec<(u32, Vec<u32>)> =
                    frozen.iter().map(|(k, items)| (k.into_raw().into_u32(), items.to_vec())).collect();
                assert_eq!(listed, model.clone().into_iter().collect::<Vec<_>>());
                for k in 0..40 {
                    assert_eq!(frozen.contains_key(&key(k)), model.contains_key(&k));
                    let twice = doubled.get(&key(k)).map(|it| it.to_vec());
                    assert_eq!(twice, model.get(&k).map(|it| it.iter().map(|it| it * 2).collect()));
                }
                let again = Csr::<Key, u32>::from_parts(
                    frozen.present().to_vec(),
                    frozen.ends().into(),
                    frozen.items().into(),
                )
                .unwrap();
                assert!(again == frozen);
            }
        })
    }

    #[test]
    fn presence_places_agree_with_counting_the_keys_before() {
        for seed in 0..32 {
            let keys: std::collections::BTreeSet<u32> =
                writes(seed, 120, 700, 1).into_iter().map(|(k, _)| k).collect();
            let words: BitSet = keys.iter().map(|&k| k as usize).collect();
            let present = Presence::new(words.into_words()).unwrap();
            assert_eq!(present.count(), keys.len());
            for k in 0..800u32 {
                let naive = keys.contains(&k).then(|| keys.range(..k).count());
                assert_eq!(present.place(k as usize), naive);
            }
            let listed: Vec<(usize, usize)> = present.keys().collect();
            let model: Vec<(usize, usize)> = keys.iter().enumerate().map(|(at, &k)| (k as usize, at)).collect();
            assert_eq!(listed, model);
            let backward: Vec<(usize, usize)> = present.keys().rev().collect();
            assert_eq!(backward, model.into_iter().rev().collect::<Vec<_>>());
        }
        assert!(Presence::new(vec![1, 0]).is_err());
        assert_eq!(Presence::new(Vec::new()).unwrap().count(), 0);
    }

    #[test]
    fn a_csr_written_in_another_order_is_the_same_column() {
        frame::task(crate::db::INFER_TAG, |frame| {
            let mut forward = CsrBuilder::<Key, u32>::new_in(frame);
            forward.insert(key(1), [1, 2]);
            forward.insert(key(5), [5]);
            let mut backward = CsrBuilder::<Key, u32>::new_in(frame);
            backward.insert(key(5), [9]);
            backward.push(key(1), 1);
            backward.insert(key(5), [5]);
            backward.push(key(1), 2);
            assert!(forward.freeze() == backward.freeze());
        })
    }

    #[test]
    fn a_list_removed_at_the_end_leaves_the_same_column() {
        frame::task(crate::db::INFER_TAG, |frame| {
            let mut forward = CsrBuilder::<Key, u32>::new_in(frame);
            forward.insert(key(1), [1]);
            let mut backward = CsrBuilder::<Key, u32>::new_in(frame);
            backward.insert(key(1), [1]);
            backward.insert(key(7), [7, 8]);
            assert!(backward.remove_in_place(&key(7)));
            let (forward, backward) = (forward.freeze(), backward.freeze());
            assert!(forward == backward);
            assert_eq!(backward.present(), &[1 << 1]);
            let trailing = Csr::<Key, u32>::from_parts(vec![1 << 1, 0], Box::new([1]), Box::new([1]));
            assert!(trailing.is_err());
            let uncovered = Csr::<Key, u32>::from_parts(vec![1 << 1], Box::new([1]), Box::new([1, 2]));
            assert!(uncovered.is_err());
            let backwards = Csr::<Key, u32>::from_parts(vec![0b110], Box::new([2, 1]), Box::new([1, 2]));
            assert!(backwards.is_err());
            let extra = Csr::<Key, u32>::from_parts(vec![1 << 1], Box::new([0, 1]), Box::new([1]));
            assert!(extra.is_err());
        })
    }

    #[test]
    fn a_column_ending_in_a_key_without_a_value_is_refused() {
        let types = &crate::next_solver::default_types().types;
        assert!(TyColumn::<Key>::from_parts(Box::new([0, NONE]), Box::new([types.i32.store()])).is_err());
        assert!(IndexColumn::<Key, u32>::from_parts(vec![1, 0], Box::new([5])).is_err());
        assert!(ModeColumn::<Key>::from_codes(Box::new([0, MODE_NONE])).is_err());
        assert!(ModeColumn::<Key>::from_codes(Box::new([MODE_NONE, 3])).is_ok());
    }

    #[test]
    fn index_column_takes_back_its_parts_and_refuses_others() {
        let column = IndexColumn::from_entries([(key(4), 40u32), (key(1), 10), (key(9), 90)]);
        let again = IndexColumn::<Key, u32>::from_parts(column.present().to_vec(), column.rows().into()).unwrap();
        assert!(again == column);
        assert_eq!(column.present(), &[1 << 1 | 1 << 4 | 1 << 9]);
        assert_eq!(column.rows(), &[10, 40, 90]);
        assert!(IndexColumn::<Key, u32>::from_parts(vec![0b101], Box::new([1])).is_err());
        assert!(IndexColumn::<Key, u32>::from_parts(vec![0b1], Box::new([1, 2])).is_err());
        assert!(IndexColumn::<Key, u32>::from_parts(vec![0, 0], Box::new([])).is_err());
    }

    #[test]
    fn field_column_takes_back_its_parts_and_refuses_others() {
        use blaze_query::db::FromId;
        let id = |index: u32| blaze_query::db::Id::from_u32(index + (2 << 20));
        let field = |local: u32| FieldId {
            parent: hir_def::StructId::from_id(id(3)).into(),
            local_id: Idx::from_raw(RawIdx::from_u32(local)),
        };
        let tuple_field = |tuple: u32, index: u32| TupleFieldId { tuple: hir_def::TupleId(tuple), index };
        let expr = |index: u32| -> ExprId { Idx::from_raw(RawIdx::from_u32(index)) };
        let column = FieldColumn::from_entries([
            (expr(5), either::Either::Left(field(2))),
            (expr(1), either::Either::Right(tuple_field(0, 1))),
            (expr(3), either::Either::Left(field(0))),
        ]);
        assert_eq!(column.get(expr(3)), Some(either::Either::Left(field(0))));
        assert_eq!(column.get(expr(2)), None);
        let again = FieldColumn::from_parts(
            column.tags().into(),
            column.data().into(),
            column.fields().into(),
            column.tuple_fields().into(),
        )
        .unwrap();
        assert!(again == column);
        let parts = |tags: &[u8], data: &[u32]| {
            FieldColumn::from_parts(tags.into(), data.into(), Box::new([field(0)]), Box::new([]))
        };
        assert!(parts(&[FIELD], &[0]).is_ok());
        assert!(parts(&[FIELD, FIELD_NONE], &[0, 0]).is_err());
        assert!(parts(&[FIELD], &[1]).is_err());
        assert!(parts(&[FIELD], &[0, 0]).is_err());
        assert!(parts(&[FIELD_NONE, FIELD], &[1, 0]).is_err());
        assert!(parts(&[7], &[0]).is_err());
        assert!(parts(&[], &[]).is_err());
    }

    #[test]
    fn an_empty_list_is_not_an_absent_one() {
        frame::task(crate::db::INFER_TAG, |frame| {
            let mut builder = CsrBuilder::<Key, u32>::new_in(frame);
            builder.ensure(key(3));
            let frozen = builder.freeze();
            assert!(frozen.contains_key(&key(3)));
            assert_eq!(frozen.get(&key(3)).map(|it| it.len()), Some(0));
            assert!(!frozen.contains_key(&key(2)));
            assert!(frozen.get(&key(2)).is_none());
        })
    }

    #[test]
    fn idx_set_agrees_with_a_set() {
        for seed in 0..32 {
            let mut model = std::collections::BTreeSet::new();
            let mut set = IdxSet::<Key>::default();
            for (k, _) in writes(seed, 300, 700, 1) {
                assert_eq!(set.insert(key(k)), model.insert(k));
            }
            for k in 0..800 {
                assert_eq!(set.contains(&key(k)), model.contains(&k));
            }
            let listed: Vec<u32> = set.iter().map(|k| k.into_raw().into_u32()).collect();
            assert_eq!(listed, model.iter().copied().collect::<Vec<_>>());
            assert_eq!(set.len(), model.len());
            assert!(set.words().last().is_none_or(|&word| word != 0));
        }
    }

    #[test]
    fn mode_column_round_trips_every_mode() {
        let modes: Vec<(Key, BindingMode)> =
            BINDING_MODES.iter().enumerate().map(|(at, &mode)| (key(at as u32 * 2), mode)).collect();
        let column = ModeColumn::from_entries(modes.iter().copied());
        for (k, mode) in &modes {
            assert_eq!(column.get(*k), Some(mode));
            assert_eq!(column[*k], *mode);
        }
        assert_eq!(column.get(key(1)), None);
        assert_eq!(column.get(key(100)), None);
        for (at, &mode) in BINDING_MODES.iter().enumerate() {
            assert_eq!(mode_code(mode) as usize, at);
        }
        assert!(ModeColumn::<Key>::from_codes(Box::new([6])).is_err());
    }

    #[test]
    fn sorted_map_agrees_with_a_map() {
        let model: BTreeMap<u32, u32> = writes(7, 100, 1000, 1000).into_iter().collect();
        let mut shuffled: Vec<(u32, u32)> = model.iter().map(|(k, v)| (*k, *v)).collect();
        shuffled.reverse();
        let map = SortedMap::from_entries(shuffled);
        for k in 0..1000 {
            assert_eq!(map.get(&k), model.get(&k));
        }
        let listed: Vec<(u32, u32)> = map.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(listed, model.into_iter().collect::<Vec<_>>());
        assert!(SortedMap::from_sorted(vec![(2, 0), (1, 0)]).is_err());
    }

    #[test]
    fn var_set_lists_what_it_holds() {
        let mut set = BitSet::<u32>::default();
        for var in [0, 63, 64, 200, 3, 63] {
            set.insert_growing(var);
        }
        assert_eq!(set.iter().collect::<Vec<_>>(), vec![0, 3, 63, 64, 200]);
        for var in 0..300 {
            assert_eq!(set.contains(var), [0, 3, 63, 64, 200].contains(&var));
        }
    }

    /// The builders as they were before they moved onto the frame, to hold the new ones to.
    mod before {
        use blaze_hash::HashMap;

        use super::super::*;

        fn dense<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> Vec<Option<V>> {
            let mut dense: Vec<Option<V>> = Vec::new();
            for (key, value) in entries {
                let index = key.to_index();
                if index >= dense.len() {
                    dense.resize_with(index + 1, || None);
                }
                dense[index] = Some(value);
            }
            dense
        }

        pub(super) fn ty_column<K: DenseKey>(entries: impl IntoIterator<Item = (K, StoredTy)>) -> TyColumn<K> {
            let mut by_ty: HashMap<StoredTy, u32> = HashMap::default();
            let mut tys = Vec::new();
            let at = dense(entries)
                .into_iter()
                .map(|ty| match ty {
                    None => NONE,
                    Some(ty) => *by_ty.entry(ty).or_insert_with_key(|ty| {
                        tys.push(*ty);
                        place(tys.len() - 1)
                    }),
                })
                .collect();
            TyColumn { at, tys: tys.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn index_column<K: DenseKey, V>(entries: impl IntoIterator<Item = (K, V)>) -> IndexColumn<K, V> {
            let (mut words, mut rows) = (BitSet::<usize>::default(), Vec::new());
            for (index, row) in dense(entries).into_iter().enumerate() {
                if let Some(row) = row {
                    words.insert_growing(index);
                    rows.push(row);
                }
            }
            let present = Presence::new(words.into_words()).unwrap();
            IndexColumn { present, rows: rows.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn mode_column<K: DenseKey>(entries: impl IntoIterator<Item = (K, BindingMode)>) -> ModeColumn<K> {
            let codes = dense(entries).into_iter().map(|mode| mode.map_or(MODE_NONE, mode_code));
            ModeColumn { codes: codes.collect(), key: PhantomData }
        }

        pub(super) fn csr<K: DenseKey, T: Clone>(ranges: &[Range], items: &[T]) -> Csr<K, T> {
            let mut words = BitSet::<usize>::default();
            let mut ends = Vec::new();
            let mut next = 0usize;
            for (index, range) in ranges.iter().enumerate() {
                if range.is_absent() {
                    continue;
                }
                words.insert_growing(index);
                next += range.len as usize;
                ends.push(place(next));
            }
            let mut flat = Vec::with_capacity(next);
            for range in ranges.iter().filter(|range| !range.is_absent()) {
                flat.extend(items[range.slice()].iter().cloned());
            }
            let present = Presence::new(words.into_words()).unwrap();
            Csr { present, ends: ends.into_boxed_slice(), items: flat.into_boxed_slice(), key: PhantomData }
        }

        pub(super) fn sorted_map<K: Ord, V>(entries: impl IntoIterator<Item = (K, V)>) -> SortedMap<K, V> {
            let mut entries: Vec<(K, V)> = entries.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            SortedMap { entries: entries.into_boxed_slice() }
        }
    }

    #[test]
    fn frame_builders_make_the_columns_the_old_builders_made() {
        let types = &crate::next_solver::default_types().types;
        let palette = [types.i32, types.u8, types.bool, types.unit, types.never];
        for seed in 0..32 {
            frame::task(crate::db::INFER_TAG, |frame| {
                let mut rows = ArenaMap::default();
                let mut tys = ArenaMap::default();
                let mut modes = ArenaMap::default();
                let mut dense_rows = DenseMap::<Key, u32>::new_in(frame);
                let mut dense_tys = DenseMap::<Key, StoredTy>::new_in(frame);
                let mut dense_modes = DenseMap::<Key, BindingMode>::new_in(frame);
                // Made at more keys than are written, as inference makes them at the node count.
                dense_rows.reserve_keys(400);
                dense_tys.reserve_keys(400);
                dense_modes.reserve_keys(400);
                for (k, v) in writes(seed, 200, 300, 1000) {
                    let ty = palette[v as usize % palette.len()].store();
                    let mode = BINDING_MODES[v as usize % BINDING_MODES.len()];
                    assert_eq!(dense_rows.insert(key(k), v), rows.insert(key(k), v));
                    dense_tys.insert(key(k), ty);
                    tys.insert(key(k), ty);
                    dense_modes.insert(key(k), mode);
                    modes.insert(key(k), mode);
                }
                for k in 0..450 {
                    assert_eq!(dense_rows.get(key(k)), rows.get(key(k)));
                    assert_eq!(dense_rows.contains_idx(key(k)), rows.contains_idx(key(k)));
                }
                assert!(dense_rows.iter().map(|(k, &v)| (k, v)).eq(rows.iter().map(|(k, &v)| (k, v))));
                *dense_rows.get_mut(key(7)).unwrap_or(&mut 0) += 1;
                if let Some(row) = rows.get_mut(key(7)) {
                    *row += 1;
                }
                let drained: Vec<(Key, u32)> = DenseMap::<Key, u32>::drain(&mut dense_rows.clone_for_test()).collect();
                assert_eq!(drained, rows.iter().map(|(k, &v)| (k, v)).collect::<Vec<_>>());
                assert!(IndexColumn::from_dense(dense_rows.slots_mut()) == before::index_column(rows));
                assert!(TyColumn::from_dense(frame, dense_tys.slots_mut()) == before::ty_column(tys));
                assert!(ModeColumn::from_dense(dense_modes.slots()) == before::mode_column(modes));

                let mut builder = CsrBuilder::<Key, u32>::new_in(frame);
                builder.reserve_keys(64);
                let mut set = FrameIdxSet::<Key>::new_in(frame);
                set.reserve_keys_for_test(700);
                let mut model_set = std::collections::BTreeSet::new();
                for (step, (k, v)) in writes(seed, 300, 40, 1000).into_iter().enumerate() {
                    match step % 3 {
                        0 => builder.insert(key(k), [v, v + 1]),
                        1 => _ = builder.remove_in_place(&key(k)),
                        _ => builder.push(key(k), v),
                    }
                    assert_eq!(set.insert(key(v % 700)), model_set.insert(v % 700));
                }
                let old = before::csr::<Key, u32>(&builder.ranges, &builder.items);
                assert!(builder.freeze() == old);
                let frozen = set.freeze();
                assert!(frozen == model_set.iter().map(|&k| key(k)).collect::<IdxSet<Key>>());
                let words = frozen.words.into_words();
                assert_eq!(words.len(), words.capacity(), "made at its length");

                let entries: Vec<(u32, u32)> =
                    writes(seed, 100, 1000, 1000).into_iter().collect::<BTreeMap<_, _>>().into_iter().rev().collect();
                assert!(SortedMap::from_entries_in(frame, entries.iter().copied()) == before::sorted_map(entries.clone()));
            });
        }
    }
}
