//! Grouping by a dense key: a stable counting sort of `(row, item)` pairs into compressed sparse
//! rows, row `r`'s items being `items[starts[r]..starts[r + 1]]` in the order their pairs came.
//!
//! The pairs are walked twice and never stored: once to count each row (prefix-summed into
//! `starts`), once to place each item at its row's cursor. Nothing is initialised before it is
//! placed, and a rebuild in a [`Csr`] and a cursor buffer that have the room allocates nothing.
//! A second walk that yields other rows than the first stops with a panic, never leaving an
//! item unplaced.
//!
//! Cost: two reads of each pair, one write of each item and two of each row's counter; it is
//! the radix sort's single pass with the digit as wide as the rows.

use core::{alloc::Allocator, fmt};
use std::alloc::Global;

#[cfg(test)]
mod tests;

/// Lists of items by row, in compressed sparse row form, both columns in `A` (a query's frame,
/// say). `A` has no default: each use names where the columns live.
pub struct Csr<T, A: Allocator> {
    /// One more than the rows, or empty for no rows: row `r` is `starts[r]..starts[r + 1]`.
    starts: Vec<u32, A>,
    items: Vec<T, A>,
}

/// No rows, in `A`'s default value: for an allocator that is a handle with one value.
impl<T, A: Allocator + Clone + Default> Default for Csr<T, A> {
    fn default() -> Csr<T, A> {
        Csr::new_in(A::default())
    }
}

/// Equal when the rows are, wherever either is allocated.
impl<T: PartialEq, A: Allocator, B: Allocator> PartialEq<Csr<T, B>> for Csr<T, A> {
    fn eq(&self, other: &Csr<T, B>) -> bool {
        self.starts == other.starts && self.items == other.items
    }
}

impl<T: Eq, A: Allocator> Eq for Csr<T, A> {}

impl<T: fmt::Debug, A: Allocator> fmt::Debug for Csr<T, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries((0..self.rows()).map(|row| self.row(row))).finish()
    }
}

impl<T, A: Allocator + Clone> Csr<T, A> {
    /// No rows, its columns to be in `alloc`, allocating nothing yet.
    pub fn new_in(alloc: A) -> Csr<T, A> {
        Csr { starts: Vec::new_in(alloc.clone()), items: Vec::new_in(alloc) }
    }
}

impl<T> Csr<T, Global> {
    /// No rows on the global heap, allocating nothing.
    ///
    /// Deprecated: names no allocator; use [`Csr::new_in`]. Goes with its last caller,
    /// blaze-ids' many.rs.
    pub const fn new() -> Csr<T, Global> {
        Csr { starts: Vec::new_in(Global), items: Vec::new_in(Global) }
    }
}

impl<T, A: Allocator + Clone> Csr<T, A> {
    /// The lists of `rows` rows from `(row, item)` pairs, each row's items in the order of their
    /// pairs, both columns in `alloc` and the walk's cursors in `scratch` (a query's frame,
    /// say). Allocates the two columns exactly. A row at or past `rows`, or more than
    /// `u32::MAX` pairs, is a bug and panics.
    pub fn build_in<I, S>(rows: usize, pairs: I, alloc: A, scratch: S) -> Csr<T, A>
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
        S: Allocator,
    {
        let mut csr = Csr::new_in(alloc);
        csr.rebuild(rows, pairs, &mut Vec::with_capacity_in(rows, scratch));
        csr
    }
}

impl<T, A: Allocator> Csr<T, A> {
    /// [`Csr::build_in`] in place, its columns and `cursors` (left unspecified, in any allocator)
    /// reused: no allocation once they have the room.
    pub fn rebuild<I, B>(&mut self, rows: usize, pairs: I, cursors: &mut Vec<u32, B>)
    where
        I: IntoIterator<Item = (u32, T)>,
        I::IntoIter: Clone,
        B: Allocator,
    {
        group_into(rows, pairs, &mut self.starts, &mut self.items, cursors);
    }

    /// From columns of the form [`Csr::into_parts`] gives; panics when they are not one.
    pub fn from_parts(starts: Vec<u32, A>, items: Vec<T, A>) -> Csr<T, A> {
        let ends = starts.last().map_or(0, |&it| it as usize);
        assert!(
            starts.first().is_none_or(|&it| it == 0)
                && starts.windows(2).all(|it| it[0] <= it[1])
                && ends == items.len(),
            "CSR starts that are not a prefix sum over {} items",
            items.len()
        );
        Csr { starts, items }
    }

    pub fn rows(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The items of every row.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// One more than the rows (none for no rows): where each row begins, then the end.
    pub fn starts(&self) -> &[u32] {
        &self.starts
    }

    /// Every row's items, row after row.
    pub fn items(&self) -> &[T] {
        &self.items
    }

    /// Every row's items, to change in place (their rows stay as they are).
    pub fn items_mut(&mut self) -> &mut [T] {
        &mut self.items
    }

    /// Row `row`'s items; panics past the rows.
    #[inline]
    pub fn row(&self, row: usize) -> &[T] {
        &self.items[self.starts[row] as usize..self.starts[row + 1] as usize]
    }

    /// Row `row`'s items, `None` past the rows.
    #[inline]
    pub fn get(&self, row: usize) -> Option<&[T]> {
        let (&start, &end) = (self.starts.get(row)?, self.starts.get(row + 1)?);
        Some(&self.items[start as usize..end as usize])
    }

    /// Where row `row`'s items are in [`Csr::items`].
    #[inline]
    pub fn range(&self, row: usize) -> core::ops::Range<usize> {
        self.starts[row] as usize..self.starts[row + 1] as usize
    }

    /// The starts and the items, as [`Csr::from_parts`] takes them back.
    pub fn into_parts(self) -> (Vec<u32, A>, Vec<T, A>) {
        (self.starts, self.items)
    }

    /// Each row's runs of equal items left as one, in place; with each row sorted first, its
    /// items become a set.
    pub fn dedup_rows(&mut self)
    where
        T: PartialEq,
    {
        let rows = self.rows();
        let mut write = 0usize;
        for row in 0..rows {
            let (start, end) = (self.starts[row] as usize, self.starts[row + 1] as usize);
            self.starts[row] = write as u32;
            for read in start..end {
                if read == start || self.items[read] != self.items[write - 1] {
                    self.items.swap(write, read);
                    write += 1;
                }
            }
        }
        if rows > 0 {
            self.starts[rows] = write as u32;
        }
        self.items.truncate(write);
    }
}

impl<A: Allocator + Clone> Csr<u32, A> {
    /// The graph of `edges` (`(source, target)`) over `nodes` nodes, in `alloc`, with the
    /// sources by target and the walks' cursors in `scratch`: row `s` holds the targets of `s`
    /// ascending, repeats kept, which is the order a sort of the edges by `(source, target)`
    /// gives. Two counting passes, by target and then stably by source, with no comparison.
    pub fn graph_in<I, S>(nodes: usize, edges: I, alloc: A, scratch: S) -> Csr<u32, A>
    where
        I: IntoIterator<Item = (u32, u32)>,
        I::IntoIter: Clone,
        S: Allocator + Clone,
    {
        let mut graph = Csr::new_in(alloc);
        let mut cursors = Vec::new_in(scratch.clone());
        graph.rebuild_graph(nodes, edges, &mut Csr::new_in(scratch), &mut cursors);
        graph
    }
}

impl<A: Allocator> Csr<u32, A> {
    /// [`Csr::graph_in`] in place, with `by_target` and `cursors` as the reused scratch (left
    /// unspecified), each in any allocator.
    pub fn rebuild_graph<I, S: Allocator, B: Allocator>(
        &mut self,
        nodes: usize,
        edges: I,
        by_target: &mut Csr<u32, S>,
        cursors: &mut Vec<u32, B>,
    ) where
        I: IntoIterator<Item = (u32, u32)>,
        I::IntoIter: Clone,
    {
        by_target.rebuild(
            nodes,
            edges.into_iter().map(|(source, target)| (target, source)),
            cursors,
        );
        let by_target = &*by_target;
        let sources = (0..nodes as u32).flat_map(move |target| {
            by_target.row(target as usize).iter().map(move |&source| (source, target))
        });
        self.rebuild(nodes, sources, cursors);
    }
}

/// The counting sort under [`Csr::rebuild`], into bare columns: `starts` gets `rows + 1`
/// entries, `items` the items in row order, `cursors` is scratch; each in any allocator (a
/// query's frame, say).
pub fn group_into<T, I, S, V, B>(
    rows: usize,
    pairs: I,
    starts: &mut Vec<u32, S>,
    items: &mut Vec<T, V>,
    cursors: &mut Vec<u32, B>,
) where
    S: Allocator,
    V: Allocator,
    B: Allocator,
    I: IntoIterator<Item = (u32, T)>,
    I::IntoIter: Clone,
{
    let pairs = pairs.into_iter();
    starts.clear();
    starts.reserve_exact(rows + 1);
    starts.resize(rows + 1, 0);
    let mut total = 0usize;
    for (row, _) in pairs.clone() {
        assert!((row as usize) < rows, "a CSR pair's row {row} is not below its {rows} rows");
        starts[row as usize + 1] += 1;
        total += 1;
    }
    assert!(u32::try_from(total).is_ok(), "a CSR of {total} items: at most 2^32 - 1");
    for at in 1..starts.len() {
        starts[at] += starts[at - 1];
    }
    cursors.clear();
    cursors.extend_from_slice(&starts[..rows]);
    items.clear();
    items.reserve_exact(total);
    let room = &mut items.spare_capacity_mut()[..total];
    let mut placed = 0usize;
    for (row, item) in pairs {
        let row = row as usize;
        let at = cursors[row];
        if at >= starts[row + 1] {
            changed();
        }
        room[at as usize].write(item);
        cursors[row] = at + 1;
        placed += 1;
    }
    if placed != total {
        changed();
    }
    // SAFETY: every write was at a row's cursor, below that row's end, and each cursor only
    // moved up from its row's start, so the writes were at distinct places of `0..total`;
    // `placed == total` of them means every place holds an initialised item.
    unsafe { items.set_len(total) };
}

#[cold]
#[inline(never)]
fn changed() -> ! {
    panic!("a CSR's pairs gave other rows in their second walk than in their first");
}
