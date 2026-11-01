//! A table of relocations: 16-byte rows read in place, sorted by `at`, each naming its target
//! by index into the table's distinct targets (docs/v2/plan/design/data.md).
//!
//! A table has one form for what it holds ([`Relocs::from_parts`] refuses any other): rows
//! sorted stably by `at`, targets distinct and in the order rows first name them, every
//! target named, and an addend kept in [`Relocs::wide`] exactly when it doesn't fit the row.
//! So two tables of the same relocations are equal, serialize alike and hash alike.

use std::{
    alloc::{Allocator, Global},
    hash::{BuildHasher, Hash},
};

use blaze_hash::FixedState;
use blaze_mem::Frame;
use hashbrown::HashTable;
use serde::{Deserialize, Serialize};

use crate::{
    RelocKind, Target,
    content::{kind_of, kind_tag},
};

/// The row addend of a relocation whose addend is in [`Relocs::wide`].
pub const WIDE: i32 = i32::MIN;

/// One relocation as stored: 16 bytes (const-asserted), read in place.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(C)]
pub struct Reloc {
    /// From the start of the code or data it is in.
    pub at: u32,
    /// Index into [`Relocs::targets`].
    pub target: u32,
    /// The addend, or [`WIDE`] for one kept in [`Relocs::wide`] by this row's index.
    pub addend: i32,
    pub kind: RelocKind,
    #[serde(skip)]
    pad: [u8; 3],
}

const _: () = assert!(size_of::<Reloc>() == 16);

impl Reloc {
    pub const fn new(at: u32, target: u32, addend: i32, kind: RelocKind) -> Reloc {
        Reloc { at, target, addend, kind, pad: [0; 3] }
    }

    /// Its bytes, little-endian, the kind as [`crate::content`]'s tag: what is hashed and kept.
    pub fn to_le_bytes(&self) -> [u8; 16] {
        let mut out = [0; 16];
        out[..4].copy_from_slice(&self.at.to_le_bytes());
        out[4..8].copy_from_slice(&self.target.to_le_bytes());
        out[8..12].copy_from_slice(&self.addend.to_le_bytes());
        out[12] = kind_tag(self.kind);
        out
    }

    /// The row of [`Reloc::to_le_bytes`]; `None` for an unknown kind or nonzero padding.
    pub fn from_le_bytes(bytes: [u8; 16]) -> Option<Reloc> {
        if bytes[13..] != [0; 3] {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
        Some(Reloc::new(word(0), word(4), word(8) as i32, kind_of(bytes[12])?))
    }
}

/// A relocation with its target and its whole addend: what a table is made from, and what
/// reading one gives (with `T` a reference into the table).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Relocation<T = Target> {
    pub at: u32,
    pub kind: RelocKind,
    pub target: T,
    pub addend: i64,
}

/// The relocations of one function, data object or static value.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Hash, Debug)] // clone: inside `CompiledFunction` and `Static`; a build makes tables in place, tests copy them
#[serde(
    try_from = "Parts<T>",
    bound(serialize = "T: Serialize", deserialize = "T: Deserialize<'de> + Eq + Hash")
)]
pub struct Relocs<T = Target> {
    rows: Vec<Reloc>,
    targets: Vec<T>,
    /// Row index and addend of each row whose addend is [`WIDE`], by row index.
    wide: Vec<(u32, i64)>,
}

/// A [`Relocs`] borrowed: its rows, targets and wide addends where they lie (in a table, or
/// read out of stored bytes), in a table's one order.
#[derive(PartialEq, Eq, Hash, Debug)]
pub struct RelocsView<'a, T> {
    rows: &'a [Reloc],
    targets: &'a [T],
    wide: &'a [(u32, i64)],
}

impl<T> Clone for RelocsView<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for RelocsView<'_, T> {}

#[derive(Deserialize)]
struct Parts<T> {
    rows: Vec<Reloc>,
    targets: Vec<T>,
    wide: Vec<(u32, i64)>,
}

impl<T: Eq + Hash> TryFrom<Parts<T>> for Relocs<T> {
    type Error = &'static str;

    fn try_from(parts: Parts<T>) -> Result<Relocs<T>, &'static str> {
        Relocs::from_parts(parts.rows, parts.targets, parts.wide)
    }
}

impl<T> Default for Relocs<T> {
    fn default() -> Relocs<T> {
        Relocs::new()
    }
}

/// A row before its table is put in order: its target by index among the distinct ones.
#[derive(Clone, Copy)]
struct Pending {
    at: u32,
    target: u32,
    kind: RelocKind,
    addend: i64,
}

/// Distinct targets in the order first given, each found by hash.
struct Distinct<T> {
    targets: Vec<T>,
    index: HashTable<u32>,
}

impl<T: Eq + Hash> Distinct<T> {
    fn with_capacity(n: usize) -> Distinct<T> {
        Distinct { targets: Vec::with_capacity(n), index: HashTable::with_capacity(n) }
    }

    fn index(&mut self, target: T) -> u32 {
        index_in(&mut self.targets, &mut self.index, target)
    }
}

/// The index of `target` among `targets`, pushed when it is new; `index` finds each by hash.
fn index_in<T: Eq + Hash, A: Allocator>(targets: &mut Vec<T>, index: &mut HashTable<u32, A>, target: T) -> u32 {
    let hash = FixedState.hash_one(&target);
    if let Some(&at) = index.find(hash, |&at| targets[at as usize] == target) {
        return at;
    }
    let at = u32::try_from(targets.len()).expect("fewer than 2^32 targets");
    targets.push(target);
    index.insert_unique(hash, at, |&at| FixedState.hash_one(&targets[at as usize]));
    at
}

impl<T> Relocs<T> {
    pub const fn new() -> Relocs<T> {
        // alloc: none; an empty `Vec` allocates nothing
        Relocs { rows: Vec::new(), targets: Vec::new(), wide: Vec::new() }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The rows, sorted by `at`.
    pub fn rows(&self) -> &[Reloc] {
        &self.rows
    }

    /// Each target once, in the order the rows first name them.
    pub fn targets(&self) -> &[T] {
        &self.targets
    }

    /// The addends that don't fit a row, by row index.
    pub fn wide(&self) -> &[(u32, i64)] {
        &self.wide
    }

    pub fn target_of(&self, row: &Reloc) -> &T {
        &self.targets[row.target as usize]
    }

    /// The addend of the `row`th row.
    pub fn addend(&self, row: usize) -> i64 {
        match self.rows[row].addend {
            WIDE => {
                let at = self.wide.binary_search_by_key(&(row as u32), |it| it.0);
                self.wide[at.expect("a wide row has its addend")].1
            }
            narrow => i64::from(narrow),
        }
    }

    pub fn get(&self, row: usize) -> Option<Relocation<&T>> {
        let it = self.rows.get(row)?;
        Some(Relocation { at: it.at, kind: it.kind, target: self.target_of(it), addend: self.addend(row) })
    }

    pub fn iter(&self) -> Iter<'_, T> {
        Iter { rows: self.rows.iter(), targets: &self.targets, wide: &self.wide }
    }

    /// The table borrowed.
    pub fn view(&self) -> RelocsView<'_, T> {
        RelocsView { rows: &self.rows, targets: &self.targets, wide: &self.wide }
    }

    /// The parts, in the one order [`Relocs::from_parts`] takes back.
    pub fn into_parts(self) -> (Vec<Reloc>, Vec<T>, Vec<(u32, i64)>) {
        (self.rows, self.targets, self.wide)
    }

    /// Puts rows in order (stably by `at`), and targets in the order they are first named,
    /// dropping any not named. `targets` are distinct. What only the assembly needs is in
    /// `scratch`.
    fn assemble<A: Allocator + Copy>(pending: &mut [Pending], targets: Vec<T>, scratch: A) -> Relocs<T> {
        if !pending.is_sorted_by_key(|it| it.at) {
            blaze_ids::radix::sort_copy_by_key(pending, |it| it.at);
        }
        let mut order = Vec::with_capacity_in(targets.len(), scratch);
        order.resize(targets.len(), u32::MAX);
        let mut named = 0u32;
        for it in pending.iter() {
            let slot = &mut order[it.target as usize];
            if *slot == u32::MAX {
                *slot = named;
                named += 1;
            }
        }
        let in_order = named as usize == targets.len() && order.iter().enumerate().all(|(at, &it)| it == at as u32);
        let targets = if in_order {
            targets
        } else {
            let mut by_order = Vec::with_capacity_in(named as usize, scratch);
            by_order.resize(named as usize, u32::MAX);
            for (old, &new) in order.iter().enumerate() {
                if new != u32::MAX {
                    by_order[new as usize] = old as u32;
                }
            }
            let mut slots = Vec::with_capacity_in(targets.len(), scratch);
            slots.extend(targets.into_iter().map(Some));
            // alloc: the table's targets, in the order rows first name them
            by_order.iter().map(|&old| slots[old as usize].take().expect("each target once")).collect()
        };
        let mut rows = Vec::with_capacity(pending.len());
        // alloc: the table's wide addends; none when every addend fits its row
        let mut wide = Vec::new();
        for (row, it) in pending.iter().enumerate() {
            let addend = match i32::try_from(it.addend) {
                Ok(narrow) if narrow != WIDE => narrow,
                _ => {
                    wide.push((row as u32, it.addend));
                    WIDE
                }
            };
            rows.push(Reloc::new(it.at, order[it.target as usize], addend, it.kind));
        }
        Relocs { rows, targets, wide }
    }

    /// The same relocations with each distinct target mapped once by `f`; targets `f` makes
    /// equal become one.
    pub fn map<U: Eq + Hash>(&self, mut f: impl FnMut(&T) -> U) -> Relocs<U> {
        let Ok(it) = self.try_map(|it| Ok::<U, std::convert::Infallible>(f(it)));
        it
    }

    /// [`Relocs::map`], stopping at the first error.
    pub fn try_map<U: Eq + Hash, E>(&self, mut f: impl FnMut(&T) -> Result<U, E>) -> Result<Relocs<U>, E> {
        let mut distinct = Distinct::with_capacity(self.targets.len());
        let mut remap = Vec::with_capacity(self.targets.len());
        for target in &self.targets {
            remap.push(distinct.index(f(target)?));
        }
        if distinct.targets.len() == self.targets.len() {
            let targets = distinct.targets;
            // clone: the new table's rows and wide addends; `self` is borrowed
            return Ok(Relocs { rows: self.rows.clone(), targets, wide: self.wide.clone() });
        }
        let mut pending: Vec<Pending> = self
            .view()
            .iter_rows()
            .map(|(row, addend)| Pending { at: row.at, target: remap[row.target as usize], kind: row.kind, addend })
            // alloc: only when `f` makes two targets one
            .collect();
        Ok(Relocs::assemble(&mut pending, distinct.targets, Global))
    }
}

impl<T: Eq + Hash> Relocs<T> {
    /// The table of `relocs`, stopping at the first error: [`FromIterator`]'s, with what only
    /// building it needs in `frame`.
    pub fn try_from_iter_in<E>(
        frame: &Frame,
        relocs: impl IntoIterator<Item = Result<Relocation<T>, E>>,
    ) -> Result<Relocs<T>, E> {
        let relocs = relocs.into_iter();
        let len = relocs.size_hint().0;
        // alloc: the table's targets
        let mut targets = Vec::with_capacity(len);
        let mut index = HashTable::with_capacity_in(len, frame);
        let mut pending = Vec::with_capacity_in(len, frame);
        for it in relocs {
            let it = it?;
            let target = index_in(&mut targets, &mut index, it.target);
            pending.push(Pending { at: it.at, target, kind: it.kind, addend: it.addend });
        }
        Ok(Relocs::assemble(&mut pending, targets, frame))
    }
}

impl<'a, T> RelocsView<'a, T> {
    pub const fn empty() -> RelocsView<'a, T> {
        RelocsView { rows: &[], targets: &[], wide: &[] }
    }

    pub fn len(self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(self) -> bool {
        self.rows.is_empty()
    }

    /// The rows, sorted by `at`.
    pub fn rows(self) -> &'a [Reloc] {
        self.rows
    }

    /// Each target once, in the order the rows first name them.
    pub fn targets(self) -> &'a [T] {
        self.targets
    }

    /// The addends that don't fit a row, by row index.
    pub fn wide(self) -> &'a [(u32, i64)] {
        self.wide
    }

    pub fn iter(self) -> Iter<'a, T> {
        Iter { rows: self.rows.iter(), targets: self.targets, wide: self.wide }
    }

    /// The table, each part copied at its exact length.
    pub fn to_relocs(self) -> Relocs<T>
    where
        T: Clone,
    {
        // alloc: a table its caller keeps
        Relocs { rows: self.rows.to_vec(), targets: self.targets.to_vec(), wide: self.wide.to_vec() }
    }

    /// [`Relocs::try_map`] of the table viewed, its index and remap in `frame`: it allocates
    /// only the new table's parts.
    pub fn try_map_in<U: Eq + Hash, E>(
        self,
        frame: &Frame,
        mut f: impl FnMut(&'a T) -> Result<U, E>,
    ) -> Result<Relocs<U>, E> {
        // alloc: the new table's targets, at their exact length
        let mut targets = Vec::with_capacity(self.targets.len());
        let mut index = HashTable::with_capacity_in(self.targets.len(), frame);
        let mut remap = Vec::with_capacity_in(self.targets.len(), frame);
        for target in self.targets {
            remap.push(index_in(&mut targets, &mut index, f(target)?));
        }
        if targets.len() == self.targets.len() {
            // alloc: the new table's rows and wide addends, at their exact length
            return Ok(Relocs { rows: self.rows.to_vec(), targets, wide: self.wide.to_vec() });
        }
        let mut pending = Vec::with_capacity_in(self.rows.len(), frame);
        pending.extend(
            self.iter_rows()
                .map(|(row, addend)| Pending { at: row.at, target: remap[row.target as usize], kind: row.kind, addend }),
        );
        Ok(Relocs::assemble(&mut pending, targets, frame))
    }

    /// Each row with its whole addend.
    fn iter_rows(self) -> impl Iterator<Item = (&'a Reloc, i64)> {
        let mut wide = self.wide.iter();
        self.rows.iter().map(move |row| match row.addend {
            WIDE => (row, wide.next().expect("a wide row has its addend").1),
            narrow => (row, i64::from(narrow)),
        })
    }
}

impl<'a, T: Eq + Hash> RelocsView<'a, T> {
    /// A view of `rows`, `targets` and `wide` in a table's one order, else what is out of it
    /// ([`Relocs::from_parts`]'s checks and words), its index in `frame`.
    pub fn from_parts(
        rows: &'a [Reloc],
        targets: &'a [T],
        wide: &'a [(u32, i64)],
        frame: &Frame,
    ) -> Result<RelocsView<'a, T>, &'static str> {
        check_parts(rows, targets, wide, frame)?;
        Ok(RelocsView { rows, targets, wide })
    }
}

impl<T: Eq + Hash> Relocs<T> {
    /// A table of `rows`, `targets` and `wide` in their one order, else what is out of it.
    pub fn from_parts(rows: Vec<Reloc>, targets: Vec<T>, wide: Vec<(u32, i64)>) -> Result<Relocs<T>, &'static str> {
        check_parts(&rows, &targets, &wide, Global)?;
        Ok(Relocs { rows, targets, wide })
    }

    /// Adds `reloc` where its offset sorts it. Builds the table again: for tests and edits off
    /// the hot path; a table is made at once with [`FromIterator`].
    pub fn push(&mut self, reloc: Relocation<T>)
    where
        T: Clone,
    {
        let rows: Vec<Relocation<T>> = self
            .iter()
            .map(|it| Relocation {
                at: it.at,
                kind: it.kind,
                // clone: off the hot path, as above
                target: it.target.clone(),
                addend: it.addend,
            })
            .chain([reloc])
            // alloc: off the hot path, as above
            .collect();
        // alloc: off the hot path, as above
        *self = rows.into_iter().collect();
    }
}

/// [`Relocs::from_parts`]'s checks, in its order, the distinct-target index in `alloc`.
fn check_parts<T: Eq + Hash, A: Allocator>(
    rows: &[Reloc],
    targets: &[T],
    wide: &[(u32, i64)],
    alloc: A,
) -> Result<(), &'static str> {
    if !rows.is_sorted_by_key(|it| it.at) {
        return Err("relocations not sorted by offset");
    }
    let mut named = 0u32;
    let mut wide_rows = 0usize;
    for row in rows {
        if row.pad != [0; 3] {
            return Err("a relocation with nonzero padding");
        }
        match row.target.cmp(&named) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal if (named as usize) < targets.len() => named += 1,
            _ => return Err("a relocation target out of first-use order or past the end"),
        }
        wide_rows += usize::from(row.addend == WIDE);
    }
    if named as usize != targets.len() {
        return Err("a relocation target no relocation names");
    }
    let mut index = HashTable::with_capacity_in(targets.len(), alloc);
    for (at, target) in targets.iter().enumerate() {
        let hash = FixedState.hash_one(target);
        if index.find(hash, |&it: &u32| targets[it as usize] == *target).is_some() {
            return Err("a relocation target listed twice");
        }
        index.insert_unique(hash, at as u32, |&it| FixedState.hash_one(&targets[it as usize]));
    }
    if wide.len() != wide_rows {
        return Err("wide addends not one for each wide relocation");
    }
    for (at, &(row, addend)) in wide.iter().enumerate() {
        if at > 0 && wide[at - 1].0 >= row {
            return Err("wide addends out of order");
        }
        if rows.get(row as usize).is_none_or(|it| it.addend != WIDE) {
            return Err("a wide addend of a relocation that is not wide");
        }
        if i32::try_from(addend).is_ok_and(|it| it != WIDE) {
            return Err("a wide addend that fits its relocation");
        }
    }
    Ok(())
}

impl<T: Eq + Hash> FromIterator<Relocation<T>> for Relocs<T> {
    fn from_iter<I: IntoIterator<Item = Relocation<T>>>(relocs: I) -> Relocs<T> {
        let relocs = relocs.into_iter();
        let mut distinct = Distinct::with_capacity(relocs.size_hint().0);
        let mut pending: Vec<Pending> = relocs
            .map(|it| Pending { at: it.at, target: distinct.index(it.target), kind: it.kind, addend: it.addend })
            // alloc: the rows before their order; a hot caller uses `Relocs::try_from_iter_in`
            .collect();
        Relocs::assemble(&mut pending, distinct.targets, Global)
    }
}

impl<T: Eq + Hash> From<Vec<Relocation<T>>> for Relocs<T> {
    fn from(relocs: Vec<Relocation<T>>) -> Relocs<T> {
        // alloc: the table made of them
        relocs.into_iter().collect()
    }
}

/// The relocations of a table in order, each with its target and whole addend.
pub struct Iter<'a, T> {
    rows: std::slice::Iter<'a, Reloc>,
    targets: &'a [T],
    wide: &'a [(u32, i64)],
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = Relocation<&'a T>;

    fn next(&mut self) -> Option<Relocation<&'a T>> {
        let row = self.rows.next()?;
        let addend = match row.addend {
            WIDE => {
                let (&(_, addend), rest) = self.wide.split_first().expect("a wide row has its addend");
                self.wide = rest;
                addend
            }
            narrow => i64::from(narrow),
        };
        Some(Relocation { at: row.at, kind: row.kind, target: &self.targets[row.target as usize], addend })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.rows.size_hint()
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

impl<'a, T> IntoIterator for &'a Relocs<T> {
    type Item = Relocation<&'a T>;
    type IntoIter = Iter<'a, T>;

    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::ContentId;
    use blaze_mem::{Tag, frame};

    use super::*;
    use crate::InstanceKey;

    fn call(n: u8) -> Target {
        Target::Function(InstanceKey(ContentId([n; 32])))
    }

    fn reloc(at: u32, target: Target, addend: i64) -> Relocation {
        Relocation { at, kind: RelocKind::PcRel32, target, addend }
    }

    fn owned(relocs: &Relocs) -> Vec<Relocation> {
        relocs.iter().map(|it| Relocation { at: it.at, kind: it.kind, target: it.target.clone(), addend: it.addend }).collect()
    }

    #[test]
    fn a_target_is_kept_once_in_first_use_order() {
        let relocs: Relocs = vec![reloc(9, call(1), -4), reloc(1, call(2), -4), reloc(5, call(1), -4)].into();
        assert_eq!(relocs.targets(), [call(2), call(1)]);
        assert_eq!(relocs.rows().iter().map(|it| (it.at, it.target)).collect::<Vec<_>>(), [(1, 0), (5, 1), (9, 1)]);
        assert_eq!(owned(&relocs), [reloc(1, call(2), -4), reloc(5, call(1), -4), reloc(9, call(1), -4)]);
    }

    #[test]
    fn one_set_of_relocations_is_one_table_whatever_the_order() {
        let rows = [reloc(1, call(1), 0), reloc(3, call(2), 1 << 40), reloc(3, call(3), i64::from(WIDE)), reloc(7, call(1), 8)];
        let a: Relocs = rows.to_vec().into();
        let mut b: Relocs = vec![rows[3].clone(), rows[0].clone()].into();
        b.push(rows[1].clone());
        b.push(rows[2].clone());
        assert_eq!(a, b);
        assert_eq!(owned(&a), rows);
        assert_eq!(a.wide(), [(1, 1 << 40), (2, i64::from(WIDE))]);
        assert_eq!((a.addend(1), a.addend(2), a.addend(3)), (1 << 40, i64::from(WIDE), 8));
    }

    #[test]
    fn parts_out_of_their_order_are_refused() {
        let table: Relocs = vec![reloc(1, call(1), 0), reloc(2, call(2), 1 << 40), reloc(3, call(1), 0)].into();
        let (rows, targets, wide) = table.clone().into_parts();
        assert_eq!(Relocs::from_parts(rows.clone(), targets.clone(), wide.clone()), Ok(table));
        let mut unsorted = rows.clone();
        unsorted.swap(0, 2);
        assert!(Relocs::from_parts(unsorted, targets.clone(), wide.clone()).is_err());
        let mut swapped = targets.clone();
        swapped.swap(0, 1);
        let mut renamed = rows.clone();
        (renamed[0].target, renamed[1].target, renamed[2].target) = (1, 0, 1);
        assert!(Relocs::from_parts(renamed, swapped, wide.clone()).is_err());
        assert!(Relocs::from_parts(rows.clone(), vec![call(1), call(1)], wide.clone()).is_err());
        assert!(Relocs::from_parts(rows.clone(), [targets.clone(), vec![call(3)]].concat(), wide.clone()).is_err());
        assert!(Relocs::from_parts(rows.clone(), targets.clone(), Vec::new()).is_err());
        assert!(Relocs::from_parts(rows.clone(), targets.clone(), vec![(1, 4)]).is_err());
        assert!(Relocs::from_parts(rows, targets, vec![(0, 1 << 40)]).is_err());
    }

    #[test]
    fn a_row_reads_back_from_its_bytes() {
        let row = Reloc::new(7, 3, WIDE, RelocKind::TlsDescCall);
        assert_eq!(Reloc::from_le_bytes(row.to_le_bytes()), Some(row));
        let mut padded = row.to_le_bytes();
        padded[15] = 1;
        assert_eq!(Reloc::from_le_bytes(padded), None);
        let mut unknown = row.to_le_bytes();
        unknown[12] = 0xff;
        assert_eq!(Reloc::from_le_bytes(unknown), None);
    }

    #[test]
    fn mapping_merges_targets_made_equal() {
        let table: Relocs = vec![reloc(1, call(1), 0), reloc(2, call(2), 1 << 40), reloc(3, call(3), 0)].into();
        let merged = table.map(|it| match it {
            Target::Function(key) if key.0.0[0] == 3 => call(1),
            other => other.clone(),
        });
        assert_eq!(merged.targets(), [call(1), call(2)]);
        assert_eq!(owned(&merged), [reloc(1, call(1), 0), reloc(2, call(2), 1 << 40), reloc(3, call(1), 0)]);
        assert_eq!(table.map(Clone::clone), table);
    }

    #[test]
    fn a_view_maps_and_checks_as_the_table_does() {
        let table: Relocs = vec![reloc(1, call(1), 0), reloc(2, call(2), 1 << 40), reloc(3, call(3), 0)].into();
        let merge = |it: &Target| match it {
            Target::Function(key) if key.0.0[0] == 3 => Ok::<_, ()>(call(1)),
            other => Ok(other.clone()),
        };
        frame::task(Tag::Codegen, |frame| {
            assert_eq!(table.view().try_map_in(frame, merge), table.try_map(merge));
            assert_eq!(table.view().try_map_in(frame, |it| Ok::<_, ()>(it.clone())), Ok(table.clone()));
            assert_eq!(table.view().to_relocs(), table);
            let (rows, targets, wide) = table.clone().into_parts();
            assert_eq!(RelocsView::from_parts(&rows, &targets, &wide, frame), Ok(table.view()));
            let twice = [call(1), call(1), call(3)];
            assert_eq!(
                RelocsView::from_parts(&rows, &twice, &wide, frame),
                Err("a relocation target listed twice")
            );
        });
    }

    /// Reading a table, relocation by relocation, and checking stored parts as a view of it
    /// allocate nothing once the thread's frames are warm.
    #[test]
    fn reading_relocations_allocates_nothing() {
        let table: Relocs = (0..64u32)
            .map(|at| reloc(at * 4, call((at % 5) as u8), if at % 7 == 0 { 1 << 40 } else { -4 }))
            .collect();
        let (rows, targets, wide) = table.clone().into_parts();
        let read = || {
            let mut sum = 0i64;
            for it in &table {
                sum = sum.wrapping_add(it.addend).wrapping_add(i64::from(it.at));
            }
            for row in 0..table.len() {
                let it = table.get(row).expect("a row");
                sum = sum.wrapping_add(it.addend ^ table.addend(row));
            }
            let view = frame::task(Tag::Codegen, |frame| {
                RelocsView::from_parts(&rows, &targets, &wide, frame).map(|it| it.iter().count())
            });
            (sum, view)
        };
        let cold = read();
        assert_eq!(blaze_alloc_count::assert_at_most(0, "reading relocations", read), cold);
        assert_eq!(cold.1, Ok(table.len()));
    }
}
