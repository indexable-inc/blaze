//! The executor: a plan to a relation, operator by operator, each materializing its output
//! (docs/v2/plan/design/query.md, "The plan IR", "Determinism").
//!
//! Order without `order by` is stated, never a hash table's: a scan in chunk and row order; a
//! filter, project and window in their input's order; a hash join in probe order, each probe
//! row's matches in build order, then (left, full) its row alone where it had none, then (full)
//! the build rows no probe met; an aggregate's groups in order of first appearance; a set
//! operation its left's rows then its right's; a fixpoint its rounds in turn. Hash tables only
//! find candidates (rule 25).

use core::alloc::Allocator;
use core::cmp::Ordering;

use blaze_columns::{Batch, Values};
use blaze_hash::{FixedState, Map, MapOpts};
use blaze_own::Vec;

use crate::Bounds;
use crate::datum::{Datum, Store};
use crate::error::{ErrorKind, Fail, Run, fail};
use crate::plan::{Cmp, Expr, JoinKind, Key, Loc, Plan, Prune, SetKind, SortKey};
use crate::table::{Bound, Catalog, below};

/// A relation: columns of datums, each `rows` long.
#[derive(Debug)]
pub struct Rel<A: Allocator> {
    pub cols: Vec<Vec<Datum, A>, A>,
    pub rows: usize,
}

impl<A: Allocator + Copy> Rel<A> {
    pub fn new_in(width: usize, alloc: A) -> Rel<A> {
        let mut cols = Vec::with_capacity_in(width, alloc);
        for _ in 0..width {
            cols.push(Vec::new_in(alloc));
        }
        Rel { cols, rows: 0 }
    }

    pub fn width(&self) -> usize {
        self.cols.len()
    }

    pub fn get(&self, col: usize, row: usize) -> Datum {
        self.cols.get(col).and_then(|it| it.get(row)).copied().unwrap_or(Datum::Null)
    }

    pub fn push(&mut self, row: &[Datum]) {
        for (col, d) in self.cols.iter_mut().zip(row) {
            col.push(*d);
        }
        self.rows += 1;
    }

    /// Row `row` of `from` (as wide as this), appended.
    pub fn push_from(&mut self, from: &Rel<A>, row: usize) {
        for (at, col) in self.cols.iter_mut().enumerate() {
            col.push(from.get(at, row));
        }
        self.rows += 1;
    }

    pub fn copy(&self, alloc: A) -> Rel<A> {
        let mut cols = Vec::with_capacity_in(self.cols.len(), alloc);
        for col in self.cols.iter() {
            cols.push(Vec::from_slice_in(col, alloc));
        }
        Rel { cols, rows: self.rows }
    }

    /// The rows `rows` of this, in that order.
    pub fn gather(&self, rows: &[u32], alloc: A) -> Rel<A> {
        let mut cols = Vec::with_capacity_in(self.cols.len(), alloc);
        for col in self.cols.iter() {
            let mut out = Vec::with_capacity_in(rows.len(), alloc);
            for &row in rows {
                out.push(col.get(row as usize).copied().unwrap_or(Datum::Null));
            }
            cols.push(out);
        }
        Rel { cols, rows: rows.len() }
    }

    /// Row `row` as a datum list.
    pub fn row(&self, row: usize, alloc: A) -> Vec<Datum, A> {
        let mut out = Vec::with_capacity_in(self.cols.len(), alloc);
        for col in self.cols.iter() {
            out.push(col.get(row).copied().unwrap_or(Datum::Null));
        }
        out
    }
}

/// A row an expression reads its columns from.
pub trait Row {
    fn col(&self, at: u32) -> Datum;
}

/// Row `row` of a relation.
pub struct At<'r, A: Allocator> {
    pub rel: &'r Rel<A>,
    pub row: usize,
}

impl<A: Allocator + Copy> Row for At<'_, A> {
    fn col(&self, at: u32) -> Datum {
        self.rel.get(at as usize, self.row)
    }
}

/// A left row and a right row side by side; no right row is nulls.
pub struct Pair<'r, A: Allocator> {
    pub left: &'r Rel<A>,
    pub l: usize,
    pub right: &'r Rel<A>,
    pub r: Option<usize>,
}

impl<A: Allocator + Copy> Row for Pair<'_, A> {
    fn col(&self, at: u32) -> Datum {
        let at = at as usize;
        let width = self.left.width();
        if at < width {
            return self.left.get(at, self.l);
        }
        match self.r {
            Some(r) => self.right.get(at - width, r),
            None => Datum::Null,
        }
    }
}

/// No row: what a constant expression is evaluated on.
pub struct NoRow;

impl Row for NoRow {
    fn col(&self, _at: u32) -> Datum {
        Datum::Null
    }
}

/// Planted bugs for the controls (rule 53). Only tests plant one.
#[cfg(test)]
pub(crate) mod plant {
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Plant {
        Clean,
        /// Join keys treat two nulls as equal, as if every key were `is not distinct from`.
        NullKeysMatch,
    }

    std::thread_local! {
        static PLANTED: Cell<Plant> = const { Cell::new(Plant::Clean) };
    }

    pub(crate) fn nulls_match() -> bool {
        PLANTED.with(Cell::get) == Plant::NullKeysMatch
    }

    pub(crate) fn with<R>(plant: Plant, body: impl FnOnce() -> R) -> R {
        let before = PLANTED.with(|p| p.replace(plant));
        let out = body();
        PLANTED.with(|p| p.set(before));
        out
    }
}

/// Whether a key datum keeps its row out of a join: null, unless the key matches nulls.
fn skips(d: Datum, null_equal: bool) -> bool {
    #[cfg(test)]
    if plant::nulls_match() {
        return false;
    }
    d.is_null() && !null_equal
}

/// Entries of a hash index: each a key of `width` datums and the row it came from.
pub struct Index<A: Allocator + Copy> {
    map: Map<u64, u32, FixedState, A>,
    next: Vec<u32, A>,
    keys: Vec<Datum, A>,
    rows: Vec<u32, A>,
    width: usize,
}

const END: u32 = u32::MAX;

impl<A: Allocator + Copy> Index<A> {
    pub fn new_in(width: usize, alloc: A) -> Index<A> {
        Index {
            map: Map::new(MapOpts { hasher: FixedState, alloc: alloc, capacity: None }),
            next: Vec::new_in(alloc),
            keys: Vec::new_in(alloc),
            rows: Vec::new_in(alloc),
            width,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Adds `key` for `row`; entries of one hash are found newest first.
    pub fn insert(&mut self, hash: u64, key: &[Datum], row: u32) {
        let entry = u32::try_from(self.rows.len()).unwrap_or(END);
        let first = self.map.insert(hash, entry).unwrap_or(END);
        self.next.push(first);
        self.keys.extend_from_slice(key);
        self.rows.push(row);
    }

    pub fn first(&self, hash: u64) -> u32 {
        self.map.get(&hash).copied().unwrap_or(END)
    }

    pub fn next(&self, entry: u32) -> u32 {
        self.next.get(entry as usize).copied().unwrap_or(END)
    }

    pub fn key(&self, entry: u32) -> &[Datum] {
        let start = entry as usize * self.width;
        self.keys.get(start..start + self.width).unwrap_or(&[])
    }

    pub fn row(&self, entry: u32) -> u32 {
        self.rows.get(entry as usize).copied().unwrap_or(END)
    }

    /// The entry whose key is [`Store::same`] as `key`, if any.
    pub fn find_same(&self, store: &Store<A>, hash: u64, key: &[Datum]) -> Option<u32> {
        let mut at = self.first(hash);
        while at != END {
            if self.key(at).iter().zip(key).all(|(a, b)| store.same(*a, *b)) {
                return Some(at);
            }
            at = self.next(at);
        }
        None
    }
}

/// Whether two join keys match: by SQL equality, nulls only where the key says.
fn keys_match<A: Allocator + Copy>(
    store: &Store<A>,
    a: &[Datum],
    b: &[Datum],
    null_equal: &[bool],
) -> bool {
    a.iter().zip(b).zip(null_equal).all(|((x, y), &nulls)| {
        if x.is_null() || y.is_null() {
            #[cfg(test)]
            if plant::nulls_match() {
                return x.is_null() && y.is_null();
            }
            return nulls && x.is_null() && y.is_null();
        }
        store.same(*x, *y)
    })
}

/// A subquery's value, computed once a query.
pub(crate) enum SubValue<A: Allocator + Copy> {
    Scalar(Datum),
    Set { index: Index<A>, has_null: bool },
    Exists(Index<A>),
}

/// What a subquery is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubKind {
    Scalar,
    Set,
    /// Correlated by equalities: its first `keys` columns are the inner keys.
    Exists { keys: u32 },
}

#[derive(Debug)]
pub struct Sub<'s, A: Allocator> {
    pub plan: Plan<'s, A>,
    pub kind: SubKind,
    pub at: Loc,
}

/// A bound query: its plan and the plans it refers to by number.
#[derive(Debug)]
pub struct Planned<'s, A: Allocator> {
    pub root: Plan<'s, A>,
    pub shared: Vec<Plan<'s, A>, A>,
    pub subs: Vec<Sub<'s, A>, A>,
    pub fixpoints: u32,
    pub joins: u32,
    /// The output columns' names.
    pub names: Vec<&'s str, A>,
}

/// A join's built side, kept while a fixpoint runs.
pub(crate) struct Built<A: Allocator + Copy> {
    rel: Rel<A>,
    index: Index<A>,
}

/// One query's run: the store, the materialized CTEs and subqueries, and the bounds.
pub struct Ctx<'p, 's, 't, A: Allocator + Copy, B: Allocator> {
    pub(crate) planned: &'p Planned<'s, A>,
    pub(crate) catalog: &'p Catalog<'t, B, A>,
    pub store: Store<A>,
    shared: Vec<Option<Rel<A>>, A>,
    pub(crate) subs: Vec<Option<SubValue<A>>, A>,
    work: Vec<Option<Rel<A>>, A>,
    built: Vec<Option<Built<A>>, A>,
    in_fixpoint: u32,
    pub(crate) bounds: Bounds,
    pub(crate) alloc: A,
}

fn nones<T, A: Allocator + Copy>(n: usize, alloc: A) -> Vec<Option<T>, A> {
    let mut out = Vec::with_capacity_in(n, alloc);
    for _ in 0..n {
        out.push(None);
    }
    out
}

impl<'p, 's, 't, A: Allocator + Copy, B: Allocator> Ctx<'p, 's, 't, A, B> {
    pub fn new(
        planned: &'p Planned<'s, A>,
        catalog: &'p Catalog<'t, B, A>,
        store: Store<A>,
        bounds: Bounds,
        alloc: A,
    ) -> Ctx<'p, 's, 't, A, B> {
        Ctx {
            planned,
            catalog,
            store,
            shared: nones(planned.shared.len(), alloc),
            subs: nones(planned.subs.len(), alloc),
            work: nones(planned.fixpoints as usize, alloc),
            built: nones(planned.joins as usize, alloc),
            in_fixpoint: 0,
            bounds,
            alloc,
        }
    }

    pub(crate) fn store_full(&self, full: crate::datum::Full) -> Fail<'s, A> {
        Fail::nowhere(ErrorKind::Bound { what: full.what, most: full.most })
    }

    fn check_rows(&self, rows: usize, at: Option<Loc>) -> Run<'s, (), A> {
        if u64::try_from(rows).unwrap_or(u64::MAX) > self.bounds.rows {
            return Err(Fail {
                at,
                kind: ErrorKind::Bound { what: "rows in one relation", most: self.bounds.rows },
            });
        }
        Ok(())
    }

    /// Runs `plan`.
    pub fn exec(&mut self, plan: &'p Plan<'s, A>) -> Run<'s, Rel<A>, A> {
        let out = match plan {
            Plan::Scan { table, width, filter, prune } => {
                self.scan(*table, *width, filter.as_ref(), prune)?
            }
            Plan::Unit => {
                let mut rel = Rel::new_in(0, self.alloc);
                rel.rows = 1;
                rel
            }
            Plan::Filter { input, predicate } => {
                let rel = self.exec(input)?;
                self.filter(rel, predicate)?
            }
            Plan::Project { input, exprs } => {
                let rel = self.exec(input)?;
                let mut out = Rel::new_in(exprs.len(), self.alloc);
                for row in 0..rel.rows {
                    for (at, expr) in exprs.iter().enumerate() {
                        let d = self.eval(expr, &At { rel: &rel, row })?;
                        if let Some(col) = out.cols.get_mut(at) {
                            col.push(d);
                        }
                    }
                    out.rows += 1;
                }
                out
            }
            Plan::Join { kind, left, right, keys, residual, id, at } => {
                self.join(*kind, left, right, keys, residual.as_ref(), *id, *at)?
            }
            Plan::Asof { outer, left, right, keys, op, probe, build, at, .. } => {
                self.asof(*outer, left, right, keys, *op, probe, build, *at)?
            }
            Plan::Aggregate { input, keys, aggs } => {
                let rel = self.exec(input)?;
                self.aggregate(&rel, keys, aggs)?
            }
            Plan::Window { input, windows } => {
                let rel = self.exec(input)?;
                self.window(rel, windows)?
            }
            Plan::Sort { input, keys } => {
                let rel = self.exec(input)?;
                let order = self.sorted(&rel, keys)?;
                rel.gather(&order, self.alloc)
            }
            Plan::Limit { input, limit, offset } => {
                let rel = self.exec(input)?;
                let start = usize::try_from(*offset).unwrap_or(usize::MAX).min(rel.rows);
                let end = match limit {
                    Some(n) => start.saturating_add(usize::try_from(*n).unwrap_or(usize::MAX)),
                    None => rel.rows,
                }
                .min(rel.rows);
                let mut rows = Vec::with_capacity_in(end - start, self.alloc);
                for row in start..end {
                    rows.push(u32::try_from(row).unwrap_or(u32::MAX));
                }
                rel.gather(&rows, self.alloc)
            }
            Plan::Distinct { input } => {
                let rel = self.exec(input)?;
                self.distinct(&rel)
            }
            Plan::Set { kind, all, left, right } => {
                let left = self.exec(left)?;
                let right = self.exec(right)?;
                self.set(*kind, *all, left, &right)?
            }
            Plan::Shared { id } => {
                let at = *id as usize;
                if let Some(Some(rel)) = self.shared.get(at) {
                    return Ok(rel.copy(self.alloc));
                }
                let planned = self.planned;
                let Some(plan) = planned.shared.get(at) else {
                    return Err(Fail::nowhere(ErrorKind::Unsupported("a shared plan out of range")));
                };
                let rel = self.exec(plan)?;
                let copy = rel.copy(self.alloc);
                if let Some(slot) = self.shared.get_mut(at) {
                    *slot = Some(rel);
                }
                copy
            }
            Plan::Work { id } => match self.work.get(*id as usize) {
                Some(Some(rel)) => rel.copy(self.alloc),
                _ => return Err(Fail::nowhere(ErrorKind::Unsupported("a working table outside its fixpoint"))),
            },
            Plan::Fixpoint { id, name, distinct, base, step, at } => {
                self.fixpoint(*id, name, *distinct, base, step, *at)?
            }
        };
        self.check_rows(out.rows, None)?;
        Ok(out)
    }

    /// A chunk of a table as datum columns.
    fn convert(store: &mut Store<A>, batch: &Batch<B>, alloc: A) -> Result<Rel<A>, crate::datum::Full> {
        let mut rel = Rel::new_in(batch.num_columns(), alloc);
        rel.rows = batch.num_rows();
        for (at, column) in batch.columns().iter().enumerate() {
            let mut out = Vec::with_capacity_in(rel.rows, alloc);
            if let Some(lists) = column.lists() {
                let mut items = Vec::new_in(alloc);
                for row in 0..rel.rows {
                    let Some(range) = lists.row(row) else {
                        out.push(Datum::Null);
                        continue;
                    };
                    items.clear();
                    let elements = lists.elements();
                    for i in range {
                        items.push(value_at(store, elements, i)?);
                    }
                    out.push(Datum::List(store.list(&items)?));
                }
            } else {
                let values = column.buffer();
                // A dictionary's values are interned once a chunk.
                let mut dict = Vec::new_in(alloc);
                if let Values::Dict { values: texts, .. } = values {
                    for text in texts.iter() {
                        dict.push(store.intern(text)?);
                    }
                }
                for row in 0..rel.rows {
                    if column.is_null(row) {
                        out.push(Datum::Null);
                        continue;
                    }
                    let d = match values {
                        Values::Dict { keys, .. } => keys
                            .get(row)
                            .and_then(|key| dict.get(*key as usize))
                            .map_or(Datum::Null, |sym| Datum::Text(*sym)),
                        _ => value_at(store, values, row)?,
                    };
                    out.push(d);
                }
            }
            if let Some(slot) = rel.cols.get_mut(at) {
                *slot = out;
            }
        }
        Ok(rel)
    }

    fn scan(
        &mut self,
        table: u32,
        width: u32,
        filter: Option<&'p Expr<A>>,
        prune: &[Prune],
    ) -> Run<'s, Rel<A>, A> {
        let catalog = self.catalog;
        let Some((name, source)) = catalog.get(table as usize) else {
            return Err(Fail::nowhere(ErrorKind::Unsupported("a table out of the catalog")));
        };
        let mut out = Rel::new_in(width as usize, self.alloc);
        let mut columns = Vec::with_capacity_in(width as usize, self.alloc);
        for at in 0..width as usize {
            columns.push(at);
        }
        for chunk in 0..source.chunks() {
            if prune.iter().any(|it| skipped(source.stats(chunk, it.column as usize), it)) {
                continue;
            }
            let mut got: Option<Result<Rel<A>, crate::datum::Full>> = None;
            let alloc = self.alloc;
            let store = &mut self.store;
            source
                .read(chunk, &columns, &mut |batch| got = Some(Ctx::<A, B>::convert(store, batch, alloc)))
                .map_err(|it| Fail::nowhere(ErrorKind::Read { table: name, chunk: it.chunk, why: it.why }))?;
            let Some(got) = got else {
                return Err(Fail::nowhere(ErrorKind::Read { table: name, chunk, why: "the table handed over no batch" }));
            };
            let rel = got.map_err(|full| self.store_full(full))?;
            if rel.width() != out.width() {
                return Err(Fail::nowhere(ErrorKind::Read { table: name, chunk, why: "a batch of another width than the table's fields" }));
            }
            for row in 0..rel.rows {
                let keep = match filter {
                    Some(predicate) => self.truth(predicate, &At { rel: &rel, row })? == Some(true),
                    None => true,
                };
                if keep {
                    out.push_from(&rel, row);
                }
            }
            self.check_rows(out.rows, None)?;
        }
        Ok(out)
    }

    fn filter(&mut self, rel: Rel<A>, predicate: &'p Expr<A>) -> Run<'s, Rel<A>, A> {
        let mut keep = Vec::new_in(self.alloc);
        for row in 0..rel.rows {
            if self.truth(predicate, &At { rel: &rel, row })? == Some(true) {
                keep.push(u32::try_from(row).unwrap_or(u32::MAX));
            }
        }
        Ok(rel.gather(&keep, self.alloc))
    }

    /// The key datums of `exprs` on each row of `rel`, row after row.
    fn key_rows(&mut self, rel: &Rel<A>, exprs: impl Iterator<Item = &'p Expr<A>> + Clone) -> Run<'s, Vec<Datum, A>, A> {
        let mut out = Vec::new_in(self.alloc);
        for row in 0..rel.rows {
            for expr in exprs.clone() {
                out.push(self.eval(expr, &At { rel, row })?);
            }
        }
        Ok(out)
    }

    /// The build side of a join: its relation and an index of its keys, rows with a null key
    /// that matches no null left out.
    fn build(
        &mut self,
        side: &'p Plan<'s, A>,
        exprs: impl Iterator<Item = &'p Expr<A>> + Clone,
        null_equal: &[bool],
    ) -> Run<'s, Built<A>, A> {
        let rel = self.exec(side)?;
        let width = null_equal.len();
        let keys = self.key_rows(&rel, exprs)?;
        let mut index = Index::new_in(width, self.alloc);
        // Newest first in a chain, so inserting from the last row finds rows in order.
        for row in (0..rel.rows).rev() {
            let key = keys.get(row * width..row * width + width).unwrap_or(&[]);
            if key.iter().zip(null_equal).any(|(d, &nulls)| skips(*d, nulls)) {
                continue;
            }
            index.insert(self.store.hash_key(key), key, u32::try_from(row).unwrap_or(u32::MAX));
        }
        Ok(Built { rel, index })
    }

    /// The built side of join `id`: kept while a fixpoint runs and the side reads no working
    /// table, else built now.
    fn built(
        &mut self,
        id: u32,
        side: &'p Plan<'s, A>,
        exprs: impl Iterator<Item = &'p Expr<A>> + Clone,
        null_equal: &[bool],
    ) -> Run<'s, Built<A>, A> {
        if let Some(slot) = self.built.get_mut(id as usize)
            && let Some(built) = slot.take()
        {
            return Ok(built);
        }
        self.build(side, exprs, null_equal)
    }

    fn keep_built(&mut self, id: u32, side: &Plan<'s, A>, built: Built<A>) {
        if self.in_fixpoint > 0
            && !side.reads_work()
            && let Some(slot) = self.built.get_mut(id as usize)
        {
            *slot = Some(built);
        }
    }

    #[expect(clippy::too_many_arguments, reason = "a join's parts, as the plan holds them")]
    fn join(
        &mut self,
        kind: JoinKind,
        left: &'p Plan<'s, A>,
        right: &'p Plan<'s, A>,
        keys: &'p [Key<A>],
        residual: Option<&'p Expr<A>>,
        id: u32,
        at: Loc,
    ) -> Run<'s, Rel<A>, A> {
        let lrel = self.exec(left)?;
        let mut null_equal = Vec::with_capacity_in(keys.len(), self.alloc);
        for key in keys {
            null_equal.push(key.null_equal);
        }
        let built = self.built(id, right, keys.iter().map(|it| &it.right), &null_equal)?;
        let rrel = &built.rel;
        let lkeys = self.key_rows(&lrel, keys.iter().map(|it| &it.left))?;
        let width = keys.len();
        let out_width = match kind {
            JoinKind::Semi | JoinKind::Anti => lrel.width(),
            JoinKind::Inner | JoinKind::Left | JoinKind::Full => lrel.width() + rrel.width(),
        };
        let mut out = Rel::new_in(out_width, self.alloc);
        let mut matched_right = Vec::from_elem_in(false, rrel.rows, self.alloc);
        let mut candidates = Vec::new_in(self.alloc);
        for l in 0..lrel.rows {
            candidates.clear();
            if width == 0 {
                for r in 0..rrel.rows {
                    candidates.push(r);
                }
            } else {
                let key = lkeys.get(l * width..l * width + width).unwrap_or(&[]);
                if !key.iter().zip(&null_equal).any(|(d, &nulls)| skips(*d, nulls)) {
                    let mut entry = built.index.first(self.store.hash_key(key));
                    while entry != END {
                        if keys_match(&self.store, built.index.key(entry), key, &null_equal) {
                            candidates.push(built.index.row(entry) as usize);
                        }
                        entry = built.index.next(entry);
                    }
                    // Newest first in a chain: the build's order is the reverse.
                    candidates.reverse();
                }
            }
            let mut any = false;
            for &r in candidates.iter() {
                let pair = Pair { left: &lrel, l, right: rrel, r: Some(r) };
                if let Some(residual) = residual
                    && self.truth(residual, &pair)? != Some(true)
                {
                    continue;
                }
                any = true;
                if let Some(seen) = matched_right.get_mut(r) {
                    *seen = true;
                }
                match kind {
                    JoinKind::Semi | JoinKind::Anti => break,
                    JoinKind::Inner | JoinKind::Left | JoinKind::Full => {
                        push_pair(&mut out, &pair);
                    }
                }
            }
            match (kind, any) {
                (JoinKind::Semi, true) | (JoinKind::Anti, false) => out.push_from(&lrel, l),
                (JoinKind::Left | JoinKind::Full, false) => {
                    push_pair(&mut out, &Pair { left: &lrel, l, right: rrel, r: None });
                }
                _ => {}
            }
            if out.rows % 4096 == 0 {
                self.check_rows(out.rows, Some(at))?;
            }
        }
        if kind == JoinKind::Full {
            let lwidth = lrel.width();
            for (r, seen) in matched_right.iter().enumerate() {
                if *seen {
                    continue;
                }
                for (c, col) in out.cols.iter_mut().enumerate() {
                    col.push(if c < lwidth { Datum::Null } else { rrel.get(c - lwidth, r) });
                }
                out.rows += 1;
            }
        }
        self.keep_built(id, right, built);
        Ok(out)
    }

    #[expect(clippy::too_many_arguments, reason = "an asof join's parts, as the plan holds them")]
    fn asof(
        &mut self,
        outer: bool,
        left: &'p Plan<'s, A>,
        right: &'p Plan<'s, A>,
        keys: &'p [Key<A>],
        op: Cmp,
        probe: &'p Expr<A>,
        build: &'p Expr<A>,
        at: Loc,
    ) -> Run<'s, Rel<A>, A> {
        let lrel = self.exec(left)?;
        let rrel = self.exec(right)?;
        let width = keys.len();
        let rkeys = self.key_rows(&rrel, keys.iter().map(|it| &it.right).chain(core::iter::once(build)))?;
        let lkeys = self.key_rows(&lrel, keys.iter().map(|it| &it.left).chain(core::iter::once(probe)))?;
        let stride = width + 1;
        // The right rows of non-null keys and value, by key then value then row.
        let mut order = Vec::new_in(self.alloc);
        for r in 0..rrel.rows {
            let key = rkeys.get(r * stride..r * stride + stride).unwrap_or(&[]);
            if key.iter().all(|d| !d.is_null()) {
                order.push(u32::try_from(r).unwrap_or(u32::MAX));
            }
        }
        let mut mismatch = None;
        let store = &self.store;
        order.sort_unstable_by(|&a, &b| {
            let ka = rkeys.get(a as usize * stride..a as usize * stride + stride).unwrap_or(&[]);
            let kb = rkeys.get(b as usize * stride..b as usize * stride + stride).unwrap_or(&[]);
            for (x, y) in ka.iter().zip(kb) {
                match store.order(*x, *y) {
                    Ok(Ordering::Equal) => {}
                    Ok(it) => return it,
                    Err(it) => {
                        mismatch = Some(it);
                        return Ordering::Equal;
                    }
                }
            }
            a.cmp(&b)
        });
        if let Some(it) = mismatch {
            return fail(at, ErrorKind::Type { op: "asof join", left: it.left, right: it.right });
        }
        let mut out = Rel::new_in(lrel.width() + rrel.width(), self.alloc);
        for l in 0..lrel.rows {
            let key = lkeys.get(l * stride..l * stride + stride).unwrap_or(&[]);
            let mut found = None;
            if key.iter().all(|d| !d.is_null()) {
                found = self.nearest(&order, &rkeys, stride, key, op).map_err(|it| Fail {
                    at: Some(at),
                    kind: ErrorKind::Type { op: "asof join", left: it.left, right: it.right },
                })?;
            }
            if found.is_some() || outer {
                push_pair(&mut out, &Pair { left: &lrel, l, right: &rrel, r: found });
            }
        }
        Ok(out)
    }

    /// The right row nearest `key`'s value under `op` among rows of equal key.
    fn nearest(
        &self,
        order: &[u32],
        rkeys: &[Datum],
        stride: usize,
        key: &[Datum],
        op: Cmp,
    ) -> Result<Option<usize>, crate::datum::Mismatch> {
        let width = stride - 1;
        let row_key = |r: u32| rkeys.get(r as usize * stride..r as usize * stride + stride).unwrap_or(&[]);
        // Compares a right row with the probe: keys, then the value.
        let cmp = |r: u32, with_value: bool| -> Result<Ordering, crate::datum::Mismatch> {
            let k = row_key(r);
            let n = if with_value { stride } else { width };
            for (x, y) in k.iter().zip(key).take(n) {
                let it = self.store.order(*x, *y)?;
                if it != Ordering::Equal {
                    return Ok(it);
                }
            }
            Ok(Ordering::Equal)
        };
        // The first row not below the probe by `test`, by halving.
        let search = |test: &dyn Fn(u32) -> Result<bool, crate::datum::Mismatch>| -> Result<usize, crate::datum::Mismatch> {
            let (mut lo, mut hi) = (0usize, order.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if test(order.get(mid).copied().unwrap_or(0))? {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            Ok(lo)
        };
        let key_start = search(&|r| Ok(cmp(r, false)? != Ordering::Less))?;
        let key_end = search(&|r| Ok(cmp(r, false)? == Ordering::Greater))?;
        let group = order.get(key_start..key_end).unwrap_or(&[]);
        let value = |r: u32| row_key(r).get(width).copied().unwrap_or(Datum::Null);
        let probe = key.get(width).copied().unwrap_or(Datum::Null);
        // Within the group, rows are by value then row.
        let first_above = |strict: bool| -> Result<usize, crate::datum::Mismatch> {
            let (mut lo, mut hi) = (0usize, group.len());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                let it = self.store.order(value(group.get(mid).copied().unwrap_or(0)), probe)?;
                let above = if strict { it == Ordering::Greater } else { it != Ordering::Less };
                if above {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            Ok(lo)
        };
        let pick = match op {
            // left >= right: the last right value at most the probe.
            Cmp::Ge => first_above(true)?.checked_sub(1),
            Cmp::Gt => first_above(false)?.checked_sub(1),
            // left <= right: the first right value at least the probe.
            Cmp::Le => Some(first_above(false)?).filter(|it| *it < group.len()),
            Cmp::Lt => Some(first_above(true)?).filter(|it| *it < group.len()),
            Cmp::Eq | Cmp::Ne => None,
        };
        Ok(pick.and_then(|it| group.get(it)).map(|r| *r as usize))
    }

    /// The order of `rel`'s rows under `keys`: stable, ties in input order.
    pub(crate) fn sorted(&mut self, rel: &Rel<A>, keys: &'p [SortKey<A>]) -> Run<'s, Vec<u32, A>, A> {
        let mut rows = Vec::with_capacity_in(rel.rows, self.alloc);
        for row in 0..rel.rows {
            rows.push(u32::try_from(row).unwrap_or(u32::MAX));
        }
        self.sort_rows(rel, &mut rows, keys)?;
        Ok(rows)
    }

    /// Sorts `rows` of `rel` under `keys`, stably.
    pub(crate) fn sort_rows(&mut self, rel: &Rel<A>, rows: &mut [u32], keys: &'p [SortKey<A>]) -> Run<'s, (), A> {
        if keys.is_empty() || rows.len() < 2 {
            return Ok(());
        }
        let width = keys.len();
        // Key datums by row number, for the rows given.
        let mut values = Vec::from_elem_in(Datum::Null, rel.rows * width, self.alloc);
        for &row in rows.iter() {
            for (k, key) in keys.iter().enumerate() {
                let d = self.eval(&key.expr, &At { rel, row: row as usize })?;
                if let Some(slot) = values.get_mut(row as usize * width + k) {
                    *slot = d;
                }
            }
        }
        let mut mismatch = None;
        let store = &self.store;
        rows.sort_unstable_by(|&a, &b| {
            for (k, key) in keys.iter().enumerate() {
                let x = values.get(a as usize * width + k).copied().unwrap_or(Datum::Null);
                let y = values.get(b as usize * width + k).copied().unwrap_or(Datum::Null);
                let it = match (x.is_null(), y.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => if key.nulls_first { Ordering::Less } else { Ordering::Greater },
                    (false, true) => if key.nulls_first { Ordering::Greater } else { Ordering::Less },
                    (false, false) => match store.order(x, y) {
                        Ok(it) if key.descending => it.reverse(),
                        Ok(it) => it,
                        Err(it) => {
                            mismatch = Some(it);
                            Ordering::Equal
                        }
                    },
                };
                if it != Ordering::Equal {
                    return it;
                }
            }
            a.cmp(&b)
        });
        if let Some(it) = mismatch {
            let at = keys.first().map(|it| it.expr.at);
            return Err(Fail { at, kind: ErrorKind::Type { op: "order by", left: it.left, right: it.right } });
        }
        Ok(())
    }

    /// The rows of `rel` without repeats, each where it first stands.
    fn distinct(&mut self, rel: &Rel<A>) -> Rel<A> {
        let mut index = Index::new_in(rel.width(), self.alloc);
        let mut keep = Vec::new_in(self.alloc);
        for row in 0..rel.rows {
            let key = rel.row(row, self.alloc);
            let hash = self.store.hash_key(&key);
            if index.find_same(&self.store, hash, &key).is_none() {
                let row = u32::try_from(row).unwrap_or(u32::MAX);
                index.insert(hash, &key, row);
                keep.push(row);
            }
        }
        rel.gather(&keep, self.alloc)
    }

    fn set(&mut self, kind: SetKind, all: bool, left: Rel<A>, right: &Rel<A>) -> Run<'s, Rel<A>, A> {
        if left.width() != right.width() {
            return Err(Fail::nowhere(ErrorKind::Width { left: left.width(), right: right.width() }));
        }
        if kind == SetKind::Union {
            let mut out = left;
            for row in 0..right.rows {
                out.push_from(right, row);
            }
            return Ok(if all { out } else { self.distinct(&out) });
        }
        // How many times each right row stands, by its first entry.
        let mut index = Index::new_in(right.width(), self.alloc);
        let mut counts: Vec<u64, A> = Vec::new_in(self.alloc);
        for row in 0..right.rows {
            let key = right.row(row, self.alloc);
            let hash = self.store.hash_key(&key);
            match index.find_same(&self.store, hash, &key) {
                Some(entry) => {
                    if let Some(count) = counts.get_mut(entry as usize) {
                        *count += 1;
                    }
                }
                None => {
                    index.insert(hash, &key, u32::try_from(row).unwrap_or(u32::MAX));
                    counts.push(1);
                }
            }
        }
        let mut keep = Vec::new_in(self.alloc);
        for row in 0..left.rows {
            let key = left.row(row, self.alloc);
            let hash = self.store.hash_key(&key);
            let count = index.find_same(&self.store, hash, &key).and_then(|it| counts.get_mut(it as usize));
            let present = count.as_ref().is_some_and(|it| **it > 0);
            let take = match kind {
                SetKind::Intersect => present,
                SetKind::Except | SetKind::Union => !present,
            };
            if all && let Some(count) = count {
                // Each right row cancels one left row.
                *count = count.saturating_sub(1);
            }
            if take {
                keep.push(u32::try_from(row).unwrap_or(u32::MAX));
            }
        }
        let out = left.gather(&keep, self.alloc);
        Ok(if all { out } else { self.distinct(&out) })
    }

    fn fixpoint(
        &mut self,
        id: u32,
        name: &'s str,
        distinct: bool,
        base: &'p Plan<'s, A>,
        step: &'p Plan<'s, A>,
        at: Loc,
    ) -> Run<'s, Rel<A>, A> {
        let first = self.exec(base)?;
        let first = if distinct { self.distinct(&first) } else { first };
        let mut seen = Index::new_in(first.width(), self.alloc);
        if distinct {
            for row in 0..first.rows {
                let key = first.row(row, self.alloc);
                seen.insert(self.store.hash_key(&key), &key, 0);
            }
        }
        let mut result = first.copy(self.alloc);
        if let Some(slot) = self.work.get_mut(id as usize) {
            *slot = Some(first);
        }
        self.in_fixpoint += 1;
        let mut rounds = 0u64;
        let ran = loop {
            rounds += 1;
            if rounds > self.bounds.rounds {
                break Err(Fail {
                    at: Some(at),
                    kind: ErrorKind::Recursion { cte: name, what: "rounds", most: self.bounds.rounds },
                });
            }
            let new = match self.exec(step) {
                Ok(it) => it,
                Err(it) => break Err(it),
            };
            let new = if distinct {
                let mut keep = Vec::new_in(self.alloc);
                for row in 0..new.rows {
                    let key = new.row(row, self.alloc);
                    let hash = self.store.hash_key(&key);
                    if seen.find_same(&self.store, hash, &key).is_none() {
                        seen.insert(hash, &key, 0);
                        keep.push(u32::try_from(row).unwrap_or(u32::MAX));
                    }
                }
                new.gather(&keep, self.alloc)
            } else {
                new
            };
            if new.rows == 0 {
                break Ok(());
            }
            if new.width() != result.width() {
                break Err(Fail { at: Some(at), kind: ErrorKind::Width { left: result.width(), right: new.width() } });
            }
            for row in 0..new.rows {
                result.push_from(&new, row);
            }
            if u64::try_from(result.rows).unwrap_or(u64::MAX) > self.bounds.rows {
                break Err(Fail {
                    at: Some(at),
                    kind: ErrorKind::Recursion { cte: name, what: "rows", most: self.bounds.rows },
                });
            }
            if let Some(slot) = self.work.get_mut(id as usize) {
                *slot = Some(new);
            }
        };
        self.in_fixpoint -= 1;
        if self.in_fixpoint == 0 {
            for slot in self.built.iter_mut() {
                *slot = None;
            }
        }
        if let Some(slot) = self.work.get_mut(id as usize) {
            *slot = None;
        }
        ran.map(|()| result)
    }
}

fn push_pair<A: Allocator + Copy>(out: &mut Rel<A>, pair: &Pair<'_, A>) {
    for (c, col) in out.cols.iter_mut().enumerate() {
        col.push(pair.col(u32::try_from(c).unwrap_or(u32::MAX)));
    }
    out.rows += 1;
}

/// Value `row` of a column's values of numbers, booleans or plain text.
fn value_at<A: Allocator, C: Allocator>(store: &mut Store<A>, values: &Values<C>, row: usize) -> Result<Datum, crate::datum::Full>
where
    A: Copy,
{
    Ok(match values {
        Values::Bool(it) => it.get(row).map_or(Datum::Null, |b| Datum::Bool(*b)),
        Values::I32(it) => it.get(row).map_or(Datum::Null, |n| Datum::Int(i64::from(*n))),
        Values::I64(it) => it.get(row).map_or(Datum::Null, |n| Datum::Int(*n)),
        Values::U16(it) => it.get(row).map_or(Datum::Null, |n| Datum::Int(i64::from(*n))),
        Values::U32(it) => it.get(row).map_or(Datum::Null, |n| Datum::Int(i64::from(*n))),
        Values::U64(it) => it.get(row).map_or(Datum::Null, |n| Datum::from_u64(*n)),
        Values::F64(it) => it.get(row).map_or(Datum::Null, |f| Datum::Float(*f)),
        Values::Text(texts) => {
            if row < texts.len() {
                Datum::Text(store.intern(texts.get(row))?)
            } else {
                Datum::Null
            }
        }
        Values::Dict { keys, values } => match keys.get(row) {
            Some(&key) if (key as usize) < values.len() => Datum::Text(store.intern(values.get(key as usize))?),
            _ => Datum::Null,
        },
    })
}

/// Whether a chunk's statistics show `prune` holds for none of its rows.
fn skipped(stats: Option<crate::table::Stats>, prune: &Prune) -> bool {
    let Some(stats) = stats else { return false };
    let (Some(min), Some(max)) = (stats.min, stats.max) else { return false };
    let v = prune.value;
    match prune.op {
        Cmp::Eq => below(v, min) || below(max, v),
        Cmp::Lt => !below(min, v),
        Cmp::Le => below(v, min),
        Cmp::Gt => !below(v, max),
        Cmp::Ge => below(max, v),
        Cmp::Ne => false,
    }
}

impl From<f64> for Bound {
    fn from(f: f64) -> Bound {
        Bound::Float(f)
    }
}
