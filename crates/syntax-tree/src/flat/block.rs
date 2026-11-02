//! A tree's columns in one allocation: a [`Block`] is the allocation, a [`Col`] a column in it,
//! read and written as a slice. A parse or an edit makes one block for its tree, not one a
//! column, and dropping the tree frees it.
//!
//! The invariant every `unsafe` below rests on: a non-empty [`Col`] is a field of the tree (or
//! of its `Kept`) whose block it was placed in or borrowed from ([`Block::borrow`]), and stays
//! so until that tree is dropped. `FlatTree` and `Kept` implement `Drop`, so the compiler
//! refuses to move a column out of one; `flat.rs` never swaps or replaces a column of one tree
//! with another's. Columns hold values without drop glue only ([`Block::place`]), so dropping
//! a block is freeing its bytes.
//!
//! A tree read in place (`image.rs`) borrows its columns from the bytes its block holds (a
//! range of the mapped image), which are read-only: `flat.rs` writes a column only of a tree
//! it has just packed (an edit's patches, a moved part's marks, on a fresh copy), never one
//! read in place, which `FlatTree::moved` asserts ([`Block::is_held`]).
//!
//! What the safe version costs: a `Box<[T]>` per column is twelve allocations and frees a tree
//! instead of one, for every parse, every lazy node's part (most trees are small parts, where
//! the allocator's calls weigh most) and every edit, and twelve allocations' headers and
//! rounding in memory; one `Box<[u64]>` cut into typed columns needs the same pointer casts as
//! this module. Reads cost the same either way: a column derefs to a slice once, and every
//! index into it is bounds-checked as a `Box<[T]>`'s is.

use std::{
    alloc::{self, Layout},
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
    mem::{align_of, needs_drop, size_of},
    ops::{Deref, DerefMut},
    ptr::{self, NonNull},
    slice,
};

use blaze_bytes::Bytes;
use blaze_mem::Tag;

use super::budget;

/// Every block's alignment: the widest column's (`u64`).
const ALIGN: usize = 8;

/// Values any bytes are a valid one of, laid out without padding, so a column of them can be
/// borrowed from stored bytes ([`Block::borrow`]) and written as bytes ([`bytes_of`]).
///
/// # Safety
///
/// Every bit pattern of `size_of::<Self>()` bytes is a valid `Self`, `Self` has no padding and
/// no interior mutability, and its alignment is at most [`ALIGN`].
pub(super) unsafe trait Plain: Copy + 'static {}

// SAFETY: integers: every bit pattern is one, no padding.
unsafe impl Plain for u32 {}
// SAFETY: as for `u32`.
unsafe impl Plain for u64 {}

/// The stored layout of a pair of `u32`s ([`Block::borrow`] reads `(u32, u32)` columns from
/// bytes written as two `u32`s): checked here, as a tuple's layout is the compiler's choice.
const _: () = assert!(
    size_of::<(u32, u32)>() == 8
        && align_of::<(u32, u32)>() == 4
        && std::mem::offset_of!((u32, u32), 0) == 0
        && std::mem::offset_of!((u32, u32), 1) == 4
);

/// A kind's side-table entry as stored: the element's `u32`, the kind's `u16`, two bytes of
/// padding written as 0 ([`Block::borrow`] reads it; it is never written as bytes).
const _: () = assert!(
    size_of::<(u32, u16)>() == 8
        && align_of::<(u32, u16)>() == 4
        && std::mem::offset_of!((u32, u16), 0) == 0
        && std::mem::offset_of!((u32, u16), 1) == 4
);

/// Values a column may be borrowed from stored bytes of: any bytes of their size are a valid
/// one (padding included, which reads are never made of).
///
/// # Safety
///
/// As [`Plain`], except that `Self` may have padding.
pub(super) unsafe trait Viewable: Copy + 'static {}

// SAFETY: `Plain`.
unsafe impl Viewable for u32 {}
// SAFETY: `Plain`.
unsafe impl Viewable for u64 {}
// SAFETY: two `u32`s at 0 and 4 (the assert above): no padding, any bits.
unsafe impl Viewable for (u32, u32) {}
// SAFETY: a `u32` at 0 and a `u16` at 4 (the assert above), any bits; the two bytes past
// them are padding, which a `(u32, u16)` never reads.
unsafe impl Viewable for (u32, u16) {}

/// `values` as their bytes, in memory order (little-endian: `image.rs` refuses other targets).
pub(super) fn bytes_of<T: Plain>(values: &[T]) -> &[u8] {
    // SAFETY: a `Plain` has no padding (its contract), so every byte of `values` is
    // initialized, and `u8` has alignment 1; the slice covers `values`' bytes exactly and
    // borrows them for as long as `values` is.
    unsafe { slice::from_raw_parts(values.as_ptr().cast::<u8>(), size_of_val(values)) }
}

/// The bytes a block needs for `len` `T`s placed after `bytes` bytes of columns, as
/// [`Block::put`] and [`Block::fill`] place them: at the next multiple of `T`'s alignment.
pub(super) fn room<T>(bytes: usize, len: usize) -> usize {
    let size = len.checked_mul(size_of::<T>()).expect("a column of under usize::MAX bytes");
    let at = bytes.next_multiple_of(align_of::<T>());
    at.checked_add(size).expect("a block of under usize::MAX bytes")
}

/// One allocation of `size` bytes, aligned to [`ALIGN`], that columns are placed in one after
/// another: every column is placed past the last, so no two overlap. A block may also hold
/// stored bytes that columns are borrowed from ([`Block::borrow`]).
pub(super) struct Block {
    ptr: NonNull<u8>,
    size: usize,
    /// The bytes placed so far.
    filled: usize,
    /// What its bytes count against ([`budget`]).
    tag: Tag,
    /// The stored bytes columns are borrowed from, for a tree read in place.
    held: Option<Box<Bytes>>,
}

// SAFETY: a block is bytes it owns alone and never reads, and `Bytes`, which are `Send` and
// `Sync`; its columns are read through `Col`, which is `Send` and `Sync` only as its `T` is.
unsafe impl Send for Block {}
// SAFETY: as for `Send`: a `&Block` reads and writes nothing.
unsafe impl Sync for Block {}

impl Block {
    /// A block of `size` bytes ([`room`]); none is allocated for 0.
    pub(super) fn new(size: usize) -> Block {
        let tag = budget::now();
        if size == 0 {
            let ptr = NonNull::<u64>::dangling().cast();
            return Block { ptr, size, filled: 0, tag, held: None };
        }
        let layout =
            Layout::from_size_align(size, ALIGN).expect("a block of under isize::MAX bytes");
        // SAFETY: `layout`'s size is not zero (the `return` above); a zero size is undefined
        // behavior for `alloc`.
        let ptr = unsafe { alloc::alloc(layout) };
        let ptr = NonNull::new(ptr).unwrap_or_else(|| alloc::handle_alloc_error(layout));
        budget::made(tag, size);
        Block { ptr, size, filled: 0, tag, held: None }
    }

    /// A block of `size` bytes ([`room`]) holding `held`, which columns are borrowed from.
    pub(super) fn holding(size: usize, held: Bytes) -> Block {
        // Not `Block { held, ..Block::new(size) }`: that copies the new block's pointer out of a
        // temporary whose drop then frees it.
        let mut block = Block::new(size);
        // alloc: the stored bytes' owner, once per tree read in place
        block.held = Some(Box::new(held));
        block
    }

    /// The bytes it holds.
    pub(super) fn size(&self) -> usize {
        self.size
    }

    /// What its bytes count against.
    pub(super) fn tag(&self) -> Tag {
        self.tag
    }

    /// Whether its columns are borrowed from stored bytes.
    pub(super) fn is_held(&self) -> bool {
        self.held.is_some()
    }

    /// A column of the `len` `T`s at byte `at` of the stored bytes the block holds, read in
    /// place; `None` past their end, at an offset not aligned for `T`, or for a block holding
    /// none.
    pub(super) fn borrow<T: Viewable>(&self, at: usize, len: usize) -> Option<Col<T>> {
        let held: &[u8] = self.held.as_deref()?;
        let end = at.checked_add(len.checked_mul(size_of::<T>())?)?;
        let bytes = held.get(at..end)?;
        if len == 0 {
            return Some(Col::empty());
        }
        if !bytes.as_ptr().cast::<T>().is_aligned() {
            return None;
        }
        let ptr = NonNull::from(bytes).cast::<T>();
        Some(Col { ptr, len, _of: PhantomData })
    }

    /// Room for `len` `T`s past every column placed so far, aligned for `T`.
    fn place<T>(&mut self, len: usize) -> NonNull<T> {
        const { assert!(align_of::<T>() <= ALIGN && !needs_drop::<T>()) };
        let at = self.filled.next_multiple_of(align_of::<T>());
        let end = room::<T>(self.filled, len);
        assert!(end <= self.size, "a column of {len} past its block's {} bytes", self.size);
        self.filled = end;
        // SAFETY: `at <= end <= size` (the assert above): the offset is in the allocation or one
        // past its end (0 in a block of size 0, whose pointer is dangling but aligned). Past the
        // end, the pointer would be out of bounds of its allocation.
        unsafe { self.ptr.add(at) }.cast()
    }

    /// A column of a copy of `from`.
    pub(super) fn put<T: Copy>(&mut self, from: &[T]) -> Col<T> {
        let ptr = self.place::<T>(from.len());
        // SAFETY: `place` gave room for `from.len()` `T`s, aligned, past every column placed
        // before (so not overlapping `from` when it is one of them) and in another allocation
        // from any slice outside the block. Overlapping or short room would corrupt a column.
        unsafe { ptr::copy_nonoverlapping(from.as_ptr(), ptr.as_ptr(), from.len()) };
        Col { ptr, len: from.len(), _of: PhantomData }
    }

    /// A column of the slices of `parts` one after another, `len` values in all.
    pub(super) fn put_joined<'a, T: Copy + 'a>(
        &mut self,
        len: usize,
        parts: impl IntoIterator<Item = &'a [T]>,
    ) -> Col<T> {
        let ptr = self.place::<T>(len);
        let mut at = 0;
        for part in parts {
            assert!(part.len() <= len - at, "parts of more than the {len} values placed");
            // SAFETY: `at + part.len() <= len` (the assert above), so slot `at` is in the room
            // `place` gave for `len` `T`s or one past its end; past it would be out of bounds.
            let to = unsafe { ptr.add(at) };
            // SAFETY: slots `at..at + part.len()` of the room `place` gave, aligned for `T`, past
            // every column placed before and in another allocation from any slice outside the
            // block, so not overlapping `part`; no earlier part wrote them.
            unsafe { ptr::copy_nonoverlapping(part.as_ptr(), to.as_ptr(), part.len()) };
            at += part.len();
        }
        assert_eq!(at, len, "parts of fewer than the values placed");
        Col { ptr, len, _of: PhantomData }
    }

    /// A column of `len` values, the `n`th `make(n)`, made in order of `n`.
    pub(super) fn fill<T>(&mut self, len: usize, mut make: impl FnMut(usize) -> T) -> Col<T> {
        let ptr = self.place::<T>(len);
        for n in 0..len {
            let value = make(n);
            // SAFETY: `n < len`: slot `n` is in the room `place` gave for `len` `T`s.
            let slot = unsafe { ptr.add(n) };
            // SAFETY: `slot` is aligned for `T` and no column overlaps it (`place`); what it held
            // needs no drop (`place`'s assert), so overwriting it leaks nothing. If `make`
            // panics, the column is never made and its slots are never read.
            unsafe { slot.write(value) };
        }
        Col { ptr, len, _of: PhantomData }
    }
}

impl Drop for Block {
    fn drop(&mut self) {
        if self.size != 0 {
            let layout = Layout::from_size_align(self.size, ALIGN).expect("as `new` made it");
            // SAFETY: `ptr` was allocated by `new` with `layout` and is freed only here, once;
            // no column's values need dropping, and no column outlives its tree's block (the
            // module's invariant). A wrong layout or a second free corrupts the allocator.
            unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) };
            budget::freed(self.tag, self.size);
        }
    }
}

/// A column of `T`s in a [`Block`], read and written as a slice.
pub(super) struct Col<T> {
    ptr: NonNull<T>,
    len: usize,
    _of: PhantomData<T>,
}

// SAFETY: a column is `T`s its tree owns alone, as a `Box<[T]>` is.
unsafe impl<T: Send> Send for Col<T> {}
// SAFETY: as for `Send`: `&Col<T>` lends only `&[T]`.
unsafe impl<T: Sync> Sync for Col<T> {}

impl<T> Col<T> {
    /// A column of nothing, in no block.
    pub(super) const fn empty() -> Col<T> {
        Col { ptr: NonNull::dangling(), len: 0, _of: PhantomData }
    }
}

impl<T> Default for Col<T> {
    fn default() -> Col<T> {
        Col::empty()
    }
}

impl<T> Deref for Col<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        // SAFETY: the `len` `T`s at `ptr` were written by `put`, `put_joined` or `fill` (none
        // for `empty`), or are stored bytes that are a valid `T` whatever they hold (`borrow`,
        // `Viewable`), aligned, and stay allocated while this column lives (the module's
        // invariant: a block holds the stored bytes it lends). They are written only through
        // `&mut self`, and no other column overlaps them (`place`). A column moved out of its
        // tree would read freed bytes here.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for Col<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as for `deref`; `&mut self` is the only way to these `T`s, as no other column
        // overlaps them and a column is neither `Clone` nor `Copy`. A borrowed column is never
        // written (the module's invariant): its bytes are shared, read-only.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: PartialEq> PartialEq for Col<T> {
    fn eq(&self, other: &Col<T>) -> bool {
        **self == **other
    }
}

impl<T: Eq> Eq for Col<T> {}

/// As a `Box<[T]>` hashes: the slice's.
impl<T: Hash> Hash for Col<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

impl<T: fmt::Debug> fmt::Debug for Col<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

#[cfg(test)]
mod tests {
    use std::{mem::size_of, ops::Range};

    use blaze_bytes::Bytes;
    use blaze_ids::rng::SplitMix64;
    use blaze_mem::{Tag, frame, tag};

    use super::{room, Block, Col};
    use crate::{FlatTree, FlatTreeBuilder, SyntaxKind};

    /// The bytes `col` reads, as addresses.
    fn span<T>(col: &Col<T>) -> Range<usize> {
        let start = col.ptr.as_ptr() as usize;
        start..start + col.len * size_of::<T>()
    }

    /// A tree of `seed`: nodes nested up to 12 deep with kinds past a word's kind bits, tokens
    /// past a word's length bits, and top-level runs long enough that a token's parent is far
    /// back, so every column of a tree has values.
    fn seeded_tree(seed: u64) -> FlatTree {
        let mut rng = SplitMix64::new(seed);
        let long = "x".repeat(1100);
        let mut builder = FlatTreeBuilder::new();
        builder.start_node(SyntaxKind(1));
        let mut depth = 1;
        for _ in 0..rng.index(3000) {
            match rng.index(8) {
                0 if depth < 12 => {
                    builder.start_node(SyntaxKind(rng.index(1100) as u16));
                    depth += 1;
                }
                1 if depth > 1 => {
                    builder.finish_node();
                    depth -= 1;
                }
                2 => builder.token(SyntaxKind(3), &long[..1000 + rng.index(100)]),
                _ => builder.token(SyntaxKind(rng.index(1100) as u16), &long[..1 + rng.index(8)]),
            }
        }
        for _ in 0..depth {
            builder.finish_node();
        }
        builder.finish()
    }

    /// Every column of `tree` copied out, the `Vec`-per-column layout the block replaced.
    #[allow(clippy::type_complexity)]
    fn columns(
        tree: &FlatTree,
    ) -> (Vec<Vec<u32>>, Vec<Vec<u64>>, Vec<Vec<(u32, u32)>>, Vec<(u32, u16)>, Vec<u128>) {
        let kept = &tree.kept;
        let words = [&tree.words, &tree.ranks, &tree.node_parents, &tree.marks];
        (
            words.into_iter().chain([&kept.elements, &kept.ranks]).map(|it| it.to_vec()).collect(),
            vec![tree.nodes.to_vec(), kept.bits.to_vec()],
            vec![tree.far.to_vec(), tree.big.to_vec()],
            tree.kinds.to_vec(),
            kept.hashes.iter().map(|it| it.value()).collect(),
        )
    }

    /// The tree's columns lie in its block, overlap nowhere and fill it to the byte.
    fn check_placed(tree: &FlatTree, seed: u64) {
        let kept = &tree.kept;
        let mut spans = vec![
            span(&tree.words),
            span(&tree.nodes),
            span(&tree.ranks),
            span(&tree.node_parents),
            span(&tree.far),
            span(&tree.kinds),
            span(&tree.big),
            span(&tree.marks),
            span(&kept.elements),
            span(&kept.bits),
            span(&kept.ranks),
            span(&kept.hashes),
        ];
        spans.retain(|it| !it.is_empty());
        spans.sort_by_key(|it| it.start);
        let block = &tree.block;
        let start = block.ptr.as_ptr() as usize;
        let inside = start..start + block.size();
        for pair in spans.windows(2) {
            assert!(pair[0].end <= pair[1].start, "seed {seed}: columns overlap: {pair:?}");
        }
        for it in &spans {
            assert!(inside.start <= it.start && it.end <= inside.end, "seed {seed}: {it:?} outside");
        }
        assert_eq!(block.filled, block.size(), "seed {seed}: the block is filled to the byte");
    }

    /// Seeded trees, and the copies `Clone` packs into blocks of their own: each tree's columns
    /// are placed in its block alone, and the copy's are the same bytes as the original's copied
    /// out column by column (the safe reference).
    #[test]
    fn a_trees_columns_fill_its_block_and_pack_again_to_the_same_bytes() {
        let mut side_tables = [false; 4];
        for seed in 0..64 {
            let tree = seeded_tree(seed);
            let _ = tree.subtree_hash(0);
            let copy = tree.clone();
            check_placed(&tree, seed);
            check_placed(&copy, seed);
            assert_eq!(columns(&copy), columns(&tree), "seed {seed}");
            let tables = [&*tree.far, &*tree.big].map(|it| !it.is_empty());
            side_tables[0] |= tables[0];
            side_tables[1] |= tables[1];
            side_tables[2] |= !tree.kinds.is_empty();
            side_tables[3] |= !tree.kept.elements.is_empty();
        }
        let what = "the seeds reach far parents, long tokens, far kinds and kept hashes";
        assert_eq!(side_tables, [true; 4], "{what}");
    }

    /// Columns of every type a tree has, of random lengths, placed in one block in a random
    /// order: each reads back what it was made of (the safe reference: the `Vec`s it was copied
    /// from), so none overlaps another, and the block is exactly the room summed for them.
    #[test]
    fn columns_read_back_what_they_were_made_of() {
        for seed in 0..500 {
            let mut rng = SplitMix64::new(seed);
            let mut len = || rng.index(70);
            let words: Vec<u32> = (0..len()).map(|it| it as u32 ^ 0x5a5a_0000).collect();
            let wide: Vec<u64> = (0..len()).map(|it| (it as u64) << 40 | 7).collect();
            let pairs: Vec<(u32, u32)> = (0..len()).map(|it| (it as u32, !(it as u32))).collect();
            let kinds: Vec<(u32, u16)> =
                (0..len()).map(|it| (it as u32, it as u16 ^ 0x3c3c)).collect();
            let filled = len();
            let mut order = [0, 1, 2, 3, 4];
            for at in (1..order.len()).rev() {
                order.swap(at, rng.index(at + 1));
            }
            let mut size = 0;
            for &it in &order {
                size = match it {
                    0 => room::<u32>(size, words.len()),
                    1 => room::<u64>(size, wide.len()),
                    2 => room::<(u32, u32)>(size, pairs.len()),
                    3 => room::<(u32, u16)>(size, kinds.len()),
                    _ => room::<u64>(size, filled),
                };
            }
            let mut block = Block::new(size);
            let (mut a, mut b, mut c, mut d, mut e) =
                (Col::empty(), Col::empty(), Col::empty(), Col::empty(), Col::empty());
            for &it in &order {
                match it {
                    0 => a = block.put(&words),
                    1 => b = block.put(&wide),
                    2 => c = block.put(&pairs),
                    3 => d = block.put(&kinds),
                    _ => e = block.fill(filled, |n| n as u64 * 3),
                }
            }
            assert_eq!(block.filled, block.size(), "seed {seed}");
            assert_eq!((&*a, &*b, &*c, &*d), (&words[..], &wide[..], &pairs[..], &kinds[..]));
            assert!(e.iter().enumerate().all(|(n, &it)| it == n as u64 * 3), "seed {seed}");
            if let Some(first) = a.first_mut() {
                *first = 1;
                assert_eq!((&*b, &*c), (&wide[..], &pairs[..]), "a write stays in its column");
            }
        }
    }

    /// A column of joined slices is their concatenation (the safe reference), placed between
    /// two other columns it does not write into.
    #[test]
    fn a_joined_column_is_its_parts_one_after_another() {
        for seed in 0..500 {
            let mut rng = SplitMix64::new(seed ^ 0x51ce);
            let parts: Vec<Vec<u32>> = (0..rng.index(6))
                .map(|n| (0..rng.index(40)).map(|it| (n as u32) << 16 | it as u32).collect())
                .collect();
            let joined = parts.concat();
            let around = [7u64, 8, 9];
            let size = room::<u64>(room::<u32>(room::<u64>(0, 3), joined.len()), 3);
            let mut block = Block::new(size);
            let before = block.put(&around);
            let col = block.put_joined(joined.len(), parts.iter().map(|it| &it[..]));
            let after = block.put(&around);
            assert_eq!(block.filled, block.size(), "seed {seed}");
            assert_eq!((&*before, &*col, &*after), (&around[..], &joined[..], &around[..]));
        }
    }

    #[test]
    #[should_panic(expected = "parts of more than")]
    fn a_joined_column_takes_no_more_than_its_room() {
        let mut block = Block::new(room::<u32>(0, 3));
        let _ = block.put_joined(3, [&[1u32, 2][..], &[3, 4][..]]);
    }

    #[test]
    #[should_panic(expected = "past its block")]
    fn a_column_past_the_room_is_refused() {
        let mut block = Block::new(room::<u32>(0, 3));
        let _ = block.put(&[1u32, 2, 3, 4]);
    }

    /// A block holding stored bytes owns its allocation: made once and not freed while it
    /// lives (its tag's line keeps its bytes once the making thread's frees are folded, at
    /// its end; no other test of this crate makes a block of [`Tag::Fuzz`]), and the column
    /// it fills reads back what was written after other allocations of its size.
    #[test]
    fn a_holding_block_keeps_its_allocation() {
        let tag = Tag::Fuzz;
        let before = tag::snapshot().get(tag).live;
        let (block, col) = std::thread::spawn(move || {
            frame::task(tag, |_| {
                let mut block = Block::holding(room::<u64>(0, 64), Bytes::from_static(b"stored"));
                let col = block.fill(64, |n| n as u64 * 7);
                (block, col)
            })
        })
        .join()
        .expect("the thread ran");
        assert!(block.is_held());
        assert_eq!(tag::snapshot().get(tag).live, before + block.size() as u64);
        let others: Vec<Vec<u64>> = (0..16).map(|_| vec![u64::MAX; 64]).collect();
        assert!(col.iter().enumerate().all(|(n, &it)| it == n as u64 * 7), "overwritten");
        drop(others);
    }
}
