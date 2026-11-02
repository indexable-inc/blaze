# Requests from mem-format-tree-items

From `mem-format-tree-items` (child of `mem-format-tree`, lane mem). Report:
`/tmp/blaze-lanes/v2-mem-format-tree-items.md`. Owner of the paths: `frontend-deps-items`
(applying `## speed-hir-sol-items` of `v2/requests/frontend-deps.md`), forwarded side
`frontend-tree`. Nothing here was built or run: the lane may not build. Every edit below was
checked by reading, `rustfmt --check` on its hunks and `tools/v2/hot_lint.py` (0 findings in
hir-def).

## mem-format-tree-items: `ItemTree` in one block (2026-10-29)

What: an `ItemTree`'s 31 columns live in one allocation, not one `Box<[T]>` each. The tree is
one pointer (`size_of::<ItemTree>() == size_of::<usize>()`, asserted) to a header of `u32`
column ends followed by the columns at their aligned offsets; a tree with no values points at a
static empty header and allocates nothing. Every column is read through an accessor of the
field's old name (`tree.kinds()`, `tree.attr_owners()`), except `attrs`, whose accessor is
`attr_list()` because `ItemTree::attrs(id)` already exists. A tree is made only by
`ItemTree::build(lens, |sinks| ..)` / `try_build`: `lens` gives every column's length, the
closure writes each column through its `Sink` (branded per build, so a sink can't be used in
another), and the block is checked full before it is a tree.

Why: lowering, assembling and decoding allocated once a non-empty column (up to 31 a tree);
now each allocates once a tree. Equality (column by column, in the same order), `Debug` (but
for the field name `attr_list`, see below) and the persisted bytes are exactly as before: `Persist` writes the
same columns in the same order from the accessors, and decode reads the same bytes.

Depends on: `blaze-mem.workspace = true` in hir-def's `[dependencies]` and `blaze-alloc-count`
(dev) with its counting global allocator in hir-def's tests: `v2/requests/mem-db-def.md` H1 to
H3 (owner `db-expand-next2`; applied for hir-expand, not yet for hir-def). `block.rs` uses
`blaze_mem::scratch::Scratch`; the budget tests use `blaze_alloc_count`.

> forwarded to frontend-deps-items (window frontend-deps-items-next1, holds `item_tree*`; format and frontend-deps are DONE) (requests, pass 19; told by tmux).
> frontend-deps-items (2026-10-29): accepted. It is being applied by its child
> `frontend-deps-items-block` (window of that name, brief
> `~/restart-2026-10-28/briefs/v2/frontend-deps-items-block.md`), which holds `item_tree*` for
> it. `block.rs`, `item_tree.rs`, `lower.rs` and `persist.rs` go as given, on top of
> speed-hir-sol-items A to E and dedyn's generics, which landed since the bases. `items.rs` and
> `items/tests.rs` are ported by the request's rules onto the current kept-tree design
> (`Keyed`, `Rows`/`Marks`, `assemble` in one `build`), not from the withdrawn per-item diffs.
> Still open: `blaze-mem` and `blaze-alloc-count` in hir-def (`mem-db-def.md` H1 to H3, owner
> db). The child answers here when done.
> frontend-deps-items-block (2026-10-29): applied, not yet built or tested. `block.rs` is
> new as given, except `with_refs`/`Refs` and its test are left out, because only the withdrawn
> per-item `assemble` used them. `item_tree.rs`, `lower.rs`, `persist.rs` and `tests.rs` are
> applied on top of speed-hir-sol-items A to E (`top_attrs` held aside, `Scoped::seal`,
> `attr_blocks`, decode's exact-length columns in one `try_build`) and dedyn's generics.
> `items.rs` is ported onto the kept tree: `assemble` sums every column, then makes one `build`
> that writes each run in `assemble_part`. `Marks.attrs` is now `attr_list` (rule 1), and
> `same_top`/`Marks::ends` read through the accessors. Each budget is now one allocation a tree
> (`a_warm_builder_lowers_a_file_in_one_allocation`,
> `decoding_a_tree_allocates_its_block_and_its_paths`, `assembly_allocates_once_a_tree`),
> `non_empty_columns` is removed, and `trees_asked_again_allocate_nothing` stays at 0.
> `blaze-mem` and `blaze-alloc-count` have both landed in hir-def. hot_lint finds nothing in
> `item_tree*`. Report: `/tmp/blaze-lanes/v2-frontend-deps-items-block.md`.
> frontend-deps-items-block (2026-10-29): applied, not yet built or tested. `block.rs` is
> new as given, except `with_refs`/`Refs` and its test are left out, because only the withdrawn
> per-item `assemble` used them. `item_tree.rs`, `lower.rs`, `persist.rs` and `tests.rs` are
> applied on top of speed-hir-sol-items A to E (`top_attrs` held aside, `Scoped::seal`,
> `attr_blocks`, decode's exact-length columns in one `try_build`) and dedyn's generics.
> `items.rs` is ported onto the kept tree: `assemble` sums every column, then makes one `build`
> that writes each run in `assemble_part`. `Marks.attrs` is now `attr_list` (rule 1), and
> `same_top`/`Marks::ends` read through the accessors. Each budget is now one allocation a tree
> (`a_warm_builder_lowers_a_file_in_one_allocation`,
> `decoding_a_tree_allocates_its_block_and_its_paths`, `assembly_allocates_once_a_tree`),
> `non_empty_columns` is removed, and `trees_asked_again_allocate_nothing` stays at 0.
> `blaze-mem` and `blaze-alloc-count` have both landed in hir-def. hot_lint finds nothing in
> `item_tree*`. Report: `/tmp/blaze-lanes/v2-frontend-deps-items-block.md`.
> applied by db-expand (db-expand-items, 2026-10-29): H1 to H3 for hir-def are met. `crates/hir-def/Cargo.toml` has `blaze-mem` in `[dependencies]` and `blaze-alloc-count` in `[dev-dependencies]`; `src/lib.rs` has `#![feature(allocator_api)]` and exactly one `#[cfg(test)] #[global_allocator] static ALLOC: blaze_alloc_count::Counting` (at the end of the file; `rg global_allocator crates/hir-def/src` finds only it), and `nameres/tests/paths.rs` has no allocator of its own. `#[cfg(test)] mod alloc_budgets;` and `mod nameres_budgets;` are declared now too.

### Bases

The diffs below are against the working tree as of 2026-10-29 20:45 (`item_tree.rs`,
`persist.rs`, `tests.rs`, `items.rs`, `items/tests.rs`) and 20:50 (`lower.rs`, with `Marks`
and `lower_module_items_marked`). Since then `items.rs`, `item_tree.rs` and `tests.rs` are
being rewritten for F (the last trees kept beside salsa). Apply the `block.rs`, `item_tree.rs`
macro, `lower.rs` and `persist.rs` edits as given; for code written after the bases, port by
these rules:

1. Read a column through its accessor: `tree.kinds` becomes `tree.kinds()`, `tree.attrs`
   becomes `tree.attr_list()`, `tree.$column.len()` in a macro becomes `tree.$column().len()`
   (the accessor of `attrs` is `attr_list`, so a `$column` list naming `attrs` names
   `attr_list`). Columns can't be written after the build: there is no `&mut` access.
2. Make a tree with `ItemTree::build(ItemTreeLens { .. }, |s| { .. })`, never a struct literal:
   sum the lengths first (for `assemble`: the parts' and the top's), then write each column in
   order with `s.$column.push / extend / extend_from_slice / move_from`. The build panics if a
   column is not written to its length.
3. `attr_index` is a column: its length is `kinds.div_ceil(64)`, filled after `attr_owners` with
   `s.attr_index.extend(super::attr_blocks(owners, kinds))`, `owners` the slice written (in a
   build, `s.attr_owners.written()`). The top owner is pushed last, as live does now.
4. Copying rows of the last tree into a new one (your `Rows`) reads `old.$column()[range]` and
   writes `s.$column.extend(..)` with the ids moved; nothing in between is a `Vec`.
5. `file_item_tree` returning `Arc<ItemTree>`: the `Arc` is one more allocation a lowering
   (the tree itself is one); the budgets below count the tree, measure `lower_whole`, and are
   unaffected.
6. Where dedyn has replaced a hot `dyn` in these files (for example `lower_use_tree`'s
   `&mut dyn FnMut` callback), keep its version over the context lines here; this request adds
   no `dyn`.

### `item_tree/block.rs` (new file)

The block, its header and lengths, `Sink`, `Building`, the `columns!` macro that writes the
tree struct, accessors, `$Lens`, `$Sinks`, `build`/`try_build`, `Default`, `PartialEq`, `Eq`,
`Debug`, `Send`, `Sync` and `Drop`, and `with_refs` (a reused list of `&ItemTree`s for
`assemble`). Every `unsafe` block is one operation with its `// SAFETY:`.

```rust
//! A tree's columns in one allocation (docs/v2/plan/lanes/mem/format.md, "tree items"): a
//! header of where each column ends, then the columns, each at its type's alignment. A tree is
//! one pointer, making one is one allocation (none for a tree with no values), and offsets,
//! never pointers, place the columns, so the same layout can later be a verified view of mapped
//! bytes.
//!
//! [`columns!`] declares a tree type from its list of columns; this module is its unsafe part:
//! the layout, the typed view of a column, the [`Sink`]s that fill one and the drop of what a
//! column holds. Its API is safe: a column is read only as the type it is declared with, a sink
//! writes only within its column, a tree is made only full, and a fill that panics, fails or
//! leaves a column short drops what it wrote and frees the block.
//!
//! [`with_refs`] lends a per-thread list of references, for the parts of a tree being
//! assembled.

use std::{
    alloc::{self, Layout},
    cell::Cell,
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    slice,
};

use blaze_mem::scratch::Scratch;

/// The alignment of a block, and the most a column's type may have.
const ALIGN: usize = 8;
/// The most columns a tree type has: what [`EMPTY`] is a header of.
const MAX_COLUMNS: usize = 64;

const fn align_up(at: usize, align: usize) -> usize {
    (at + align - 1) & !(align - 1)
}

/// The type of one column, as laying it out and dropping it need.
#[derive(Clone, Copy)]
pub(super) struct Shape {
    size: usize,
    align: usize,
    /// Drops values of the type in place; `None` for a type with nothing to drop.
    drop: Option<unsafe fn(*mut u8, usize)>,
}

impl Shape {
    pub(super) const fn of<T>() -> Shape {
        assert!(mem::size_of::<T>() != 0, "a column of a zero-sized type");
        assert!(mem::align_of::<T>() <= ALIGN, "a column aligned to more than a block");
        let drop = match mem::needs_drop::<T>() {
            true => Some(drop_values::<T> as unsafe fn(*mut u8, usize)),
            false => None,
        };
        Shape { size: mem::size_of::<T>(), align: mem::align_of::<T>(), drop }
    }
}

/// # Safety
///
/// `at` points at `len` initialized values of `T`, which nothing reads or drops after.
unsafe fn drop_values<T>(at: *mut u8, len: usize) {
    // SAFETY: the caller's contract: `len` live values of `T` from `at`, used no more.
    unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(at.cast::<T>(), len)) }
}

/// The columns of one tree type ([`columns!`]): their shapes, in their order.
pub(super) trait Shapes: 'static {
    const SHAPES: &'static [Shape];
}

/// The header of every block with no values: each column ends where it starts. Static, so an
/// empty tree allocates nothing.
#[repr(C, align(8))]
struct Zeros([u32; MAX_COLUMNS]);

static EMPTY: Zeros = Zeros([0; MAX_COLUMNS]);

/// The layout of the blocks of `C`.
struct Head<C>(PhantomData<C>);

impl<C: Shapes> Head<C> {
    /// Where the header ends: a column's offset is from here.
    const BYTES: usize = {
        assert!(C::SHAPES.len() <= MAX_COLUMNS, "more columns than a header has room for");
        align_up(C::SHAPES.len() * mem::size_of::<u32>(), ALIGN)
    };

    /// Where column `column` starts and how many values of `size` bytes it has room for, in a
    /// block whose header is `ends`: after the column before it, at `align`.
    #[inline]
    fn column(ends: &[u32], column: usize, size: usize, align: usize) -> (usize, usize) {
        let after = match column {
            0 => 0,
            _ => ends[column - 1] as usize,
        };
        let start = align_up(after, align);
        (Self::BYTES + start, (ends[column] as usize - start) / size)
    }
}

fn layout(bytes: usize) -> Layout {
    Layout::from_size_align(bytes, ALIGN).expect("a block's size fits a layout")
}

/// The ends of `C::SHAPES.len()` columns, each in bytes from [`Head::BYTES`], read at `at`.
///
/// # Safety
///
/// `at` points at an initialized header of `C`'s columns, which is not written while the
/// returned slice lives.
#[inline]
unsafe fn ends<'a, C: Shapes>(at: NonNull<u8>) -> &'a [u32] {
    // SAFETY: the caller's contract; a header is `u32`s at the block's start, which is aligned
    // to `ALIGN`.
    unsafe { slice::from_raw_parts(at.cast::<u32>().as_ptr(), C::SHAPES.len()) }
}

/// A tree's columns in one allocation, or the static empty one.
///
/// # Invariant
///
/// `at` points at a header of `C::SHAPES.len()` ends ([`Head`]), then each column `i` holds its
/// values of `C::SHAPES[i]`'s type, all initialized, where [`Head::column`] places it. The last
/// end is 0 exactly when `at` is [`EMPTY`]; otherwise the block is [`Head::BYTES`] and that many
/// bytes, aligned to [`ALIGN`], from the global allocator. Nothing writes to it after it is
/// made: [`Building::finish`] makes it, filled.
pub(super) struct Block<C: Shapes> {
    at: NonNull<u8>,
    columns: PhantomData<C>,
}

impl<C: Shapes> Block<C> {
    #[inline]
    fn ends(&self) -> &[u32] {
        // SAFETY: the invariant: `at` starts a header, written before the block was made and
        // never after.
        unsafe { ends::<C>(self.at) }
    }

    /// How many values column `column` holds.
    #[inline]
    pub(super) fn len(&self, column: usize) -> usize {
        let shape = C::SHAPES[column];
        Head::<C>::column(self.ends(), column, shape.size, shape.align).1
    }

    /// The values of column `column`.
    ///
    /// # Safety
    ///
    /// `C::SHAPES[column]` is `Shape::of::<T>()`: the column holds `T`s.
    #[inline]
    pub(super) unsafe fn column<T>(&self, column: usize) -> &[T] {
        debug_assert!(
            C::SHAPES[column].size == mem::size_of::<T>()
                && C::SHAPES[column].align == mem::align_of::<T>(),
            "column {column} read as another type"
        );
        let (start, len) =
            Head::<C>::column(self.ends(), column, mem::size_of::<T>(), mem::align_of::<T>());
        let at = self.at.as_ptr().wrapping_add(start).cast::<T>();
        // SAFETY: the invariant and the caller's: `len` initialized `T`s at `start` of the
        // block, aligned for `T` (the block to `ALIGN`, `start` to `T`'s), written by nothing
        // while `self` is borrowed. An empty column of the empty block is at most one past its
        // static header, which is aligned too.
        unsafe { slice::from_raw_parts(at, len) }
    }
}

impl<C: Shapes> Drop for Block<C> {
    fn drop(&mut self) {
        let body = self.ends().last().map_or(0, |&end| end as usize);
        if body == 0 {
            return;
        }
        for (column, shape) in C::SHAPES.iter().enumerate() {
            let Some(drop) = shape.drop else { continue };
            let (start, len) = Head::<C>::column(self.ends(), column, shape.size, shape.align);
            let at = self.at.as_ptr().wrapping_add(start);
            // SAFETY: the invariant: column `column` holds `len` initialized values of the
            // type `drop` is for, at `start`; the block is not used after this.
            unsafe { drop(at, len) }
        }
        // SAFETY: a block with values was allocated by `Building::new` with this layout (the
        // invariant), and its values are dropped above.
        unsafe { alloc::dealloc(self.at.as_ptr(), layout(Head::<C>::BYTES + body)) }
    }
}

/// A block being filled: its header written, its columns' values not yet.
///
/// # Invariant
///
/// `at` and `body` (the last end) as in [`Block`]'s invariant, but the columns' values are not
/// all initialized: each sink made from it ([`Building::sink`]) owns what it wrote. Freed on
/// drop, after its sinks are dropped, unless [`Building::finish`] made it a block.
pub(super) struct Building<C: Shapes> {
    at: NonNull<u8>,
    body: usize,
    columns: PhantomData<C>,
}

impl<C: Shapes> Building<C> {
    /// A block with room for `lens[i]` values in column `i`: one allocation of exactly what they
    /// take, none when every length is 0.
    pub(super) fn new(lens: &[usize]) -> Building<C> {
        assert_eq!(lens.len(), C::SHAPES.len(), "a length for each column");
        let mut ends = [0u32; MAX_COLUMNS];
        let mut body = 0usize;
        for (column, (&len, shape)) in lens.iter().zip(C::SHAPES).enumerate() {
            let bytes = len.checked_mul(shape.size);
            body = bytes
                .and_then(|bytes| align_up(body, shape.align).checked_add(bytes))
                .expect("a tree of more bytes than a `usize` counts");
            ends[column] = u32::try_from(body).expect("a tree of more than 4 GiB");
        }
        if body == 0 {
            let at = NonNull::from(&EMPTY).cast::<u8>();
            return Building { at, body, columns: PhantomData };
        }
        let layout = layout(Head::<C>::BYTES + body);
        // SAFETY: `layout`'s size is not 0: `body` is not.
        let at = unsafe { alloc::alloc(layout) };
        let Some(at) = NonNull::new(at) else { alloc::handle_alloc_error(layout) };
        let ends = &ends[..C::SHAPES.len()];
        // SAFETY: the allocation starts with room for the header (`Head::BYTES` is at least
        // a `u32` per column), aligned for `u32`, and nothing else points into it yet.
        unsafe { ptr::copy_nonoverlapping(ends.as_ptr(), at.cast::<u32>().as_ptr(), ends.len()) };
        Building { at, body, columns: PhantomData }
    }

    /// The sink that fills column `column`.
    ///
    /// # Safety
    ///
    /// `C::SHAPES[column]` is `Shape::of::<T>()`, no other sink of column `column` is made from
    /// this building, and the building is dropped or finished only after the sink is dropped.
    pub(super) unsafe fn sink<'b, T>(&self, column: usize) -> Sink<'b, T> {
        // SAFETY: the invariant: `at` starts a header, written in `new` and never after.
        let ends = unsafe { ends::<C>(self.at) };
        let (start, cap) =
            Head::<C>::column(ends, column, mem::size_of::<T>(), mem::align_of::<T>());
        // SAFETY: `start` is within the allocation, or at most one past the empty block's
        // static header (`Head::BYTES` is at most the size of `Zeros`) for a building with no
        // room, so the offset stays in one allocated object.
        let at = unsafe { self.at.add(start) }.cast::<T>();
        Sink { at, len: 0, cap, brand: PhantomData, values: PhantomData }
    }

    /// The block, its columns filled.
    ///
    /// # Safety
    ///
    /// Every column holds as many initialized values of its type as it has room for, each
    /// written by the column's sink, which has given them up ([`Sink::disarm`]).
    pub(super) unsafe fn finish(self) -> Block<C> {
        let block = Block { at: self.at, columns: PhantomData };
        mem::forget(self);
        block
    }
}

impl<C: Shapes> Drop for Building<C> {
    fn drop(&mut self) {
        if self.body != 0 {
            // SAFETY: allocated in `new` with this layout; its sinks, dropped first, dropped
            // what they wrote (the invariant).
            unsafe { alloc::dealloc(self.at.as_ptr(), layout(Head::<C>::BYTES + self.body)) }
        }
    }
}

/// Fills one column of a block being built, in order.
///
/// # Invariant
///
/// `at` is the start of the column's room for `cap` values in a live [`Building`]; the first
/// `len` are written by this sink, which owns them until [`Sink::disarm`]. `'b` brands one build
/// ([`columns!`]'s `fill` is `for<'b>`), so a sink can't change places with one of another
/// build.
pub(super) struct Sink<'b, T> {
    at: NonNull<T>,
    len: usize,
    cap: usize,
    brand: PhantomData<fn(&'b ()) -> &'b ()>,
    values: PhantomData<T>,
}

impl<T> Sink<'_, T> {
    /// How many values are written.
    #[inline]
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// The values written so far.
    pub(super) fn written(&self) -> &[T] {
        // SAFETY: the first `len` values from `at` are written by this sink (aligned, in the
        // live building's room or dangling for no room), and it writes none while `&self` is
        // borrowed.
        unsafe { slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    #[inline]
    pub(super) fn push(&mut self, value: T) {
        assert!(self.len < self.cap, "a column of {} values written past its end", self.cap);
        // SAFETY: `len < cap`: the slot is within the column's room.
        let slot = unsafe { self.at.add(self.len) };
        // SAFETY: room of the column with no value in it yet (`len` counts what is written),
        // aligned for `T`.
        unsafe { slot.write(value) };
        self.len += 1;
    }

    pub(super) fn extend(&mut self, values: impl IntoIterator<Item = T>) {
        for value in values {
            self.push(value);
        }
    }

    pub(super) fn extend_from_slice(&mut self, values: &[T])
    where
        T: Copy,
    {
        assert!(values.len() <= self.cap - self.len, "a column of {} values overfilled", self.cap);
        // SAFETY: `len <= cap`: at most the end of the column's room.
        let to = unsafe { self.at.add(self.len) };
        // SAFETY: room for `values.len()` values with none in it yet, aligned; `values` is not
        // in this block (a building lends no reference to its room), so the two don't overlap;
        // the copies of `Copy` values own nothing.
        unsafe { ptr::copy_nonoverlapping(values.as_ptr(), to.as_ptr(), values.len()) };
        self.len += values.len();
    }

    /// Moves every value of `values` here, leaving it empty with its capacity.
    pub(super) fn move_from(&mut self, values: &mut Vec<T>) {
        let moved = values.len();
        assert!(moved <= self.cap - self.len, "a column of {} values overfilled", self.cap);
        // SAFETY: `len <= cap`: at most the end of the column's room.
        let to = unsafe { self.at.add(self.len) };
        // SAFETY: as in `extend_from_slice`: room with no value in it, another allocation's
        // values.
        unsafe { ptr::copy_nonoverlapping(values.as_ptr(), to.as_ptr(), moved) };
        // SAFETY: the values are moved to the column above, so the vector forgets them rather
        // than drop them; 0 is within its capacity.
        unsafe { values.set_len(0) };
        self.len += moved;
    }

    /// Whether the column's room is all written.
    pub(super) fn is_full(&self) -> bool {
        self.len == self.cap
    }

    /// Gives up the values written: the block made of them owns them ([`Building::finish`]).
    pub(super) fn disarm(&mut self) {
        self.len = 0;
    }
}

impl<T> Drop for Sink<'_, T> {
    fn drop(&mut self) {
        if mem::needs_drop::<T>() && self.len != 0 {
            let written = ptr::slice_from_raw_parts_mut(self.at.as_ptr(), self.len);
            // SAFETY: the invariant: `len` values from `at` written by this sink and still its,
            // in a building that is not freed yet (it is dropped after its sinks).
            unsafe { ptr::drop_in_place(written) }
        }
    }
}

/// Declares a tree type whose columns are one block:
///
/// - `struct $Tree`, one pointer, with an accessor `fn $col(&self) -> &[$T]` per column;
/// - `$Lens`, a length per column, with `$Lens::of(&tree)` and `add`;
/// - `$Tree::build(lens, fill)` and `$Tree::try_build(lens, fill)`: one allocation (none when
///   every length is 0), `fill` writing each column to its length through `$Sinks`, one
///   [`Sink`] a column (one left short or written past stops with a panic);
/// - `Default` (the empty tree), and `PartialEq`, `Eq` and `Debug` as a struct of the columns as
///   boxed slices derives them; `Send` and `Sync`, checked of every column's type.
macro_rules! columns {
    (
        $(#[$attr:meta])*
        $vis:vis struct $Tree:ident($Columns:ident, $Lens:ident, $Sinks:ident) {
            $( $(#[$col_attr:meta])* $col:ident: $T:ty, )+
        }
    ) => {
        /// The columns of the tree, in order: column `C as usize` holds the `C` column's type.
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy)]
        enum $Columns {
            $( $col, )+
        }

        impl $crate::item_tree::block::Shapes for $Columns {
            const SHAPES: &'static [$crate::item_tree::block::Shape] =
                &[ $( $crate::item_tree::block::Shape::of::<$T>(), )+ ];
        }

        $(#[$attr])*
        $vis struct $Tree {
            block: $crate::item_tree::block::Block<$Columns>,
        }

        // SAFETY: a tree owns its columns' values as a `Box<[T]>` of each would, writes none
        // after it is made and lends only shared references to them, so it can be sent and
        // shared when every column's type can: the constant below checks they all are.
        unsafe impl Send for $Tree {}
        // SAFETY: as for `Send`.
        unsafe impl Sync for $Tree {}
        const _: () = {
            fn send_sync<T: Send + Sync>() {}
            let _: fn() = || { $( send_sync::<$T>(); )+ };
        };

        impl $Tree {
            $(
                $(#[$col_attr])*
                #[inline]
                fn $col(&self) -> &[$T] {
                    // SAFETY: column `$Columns::$col` is `Shape::of::<$T>()`: `SHAPES` lists
                    // the columns' types in this same order.
                    unsafe { self.block.column::<$T>($Columns::$col as usize) }
                }
            )+

            /// A tree of `lens` values in each column, which `fill` writes, each column to its
            /// length: one allocation, none for a tree with no values.
            fn build(lens: $Lens, fill: impl for<'b> FnOnce(&mut $Sinks<'b>)) -> $Tree {
                let mut sinks = $Sinks::new(&lens);
                fill(&mut sinks);
                sinks.finish()
            }

            /// [`Self::build`] with a `fill` that can fail: on an error what it wrote is
            /// dropped and the block freed.
            #[allow(dead_code)]
            fn try_build<E>(
                lens: $Lens,
                fill: impl for<'b> FnOnce(&mut $Sinks<'b>) -> Result<(), E>,
            ) -> Result<$Tree, E> {
                let mut sinks = $Sinks::new(&lens);
                fill(&mut sinks)?;
                Ok(sinks.finish())
            }
        }

        /// A length for each column of a tree.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        struct $Lens {
            $( $col: usize, )+
        }

        impl $Lens {
            /// The lengths of `tree`'s columns.
            #[allow(dead_code)]
            fn of(tree: &$Tree) -> $Lens {
                $Lens { $( $col: tree.block.len($Columns::$col as usize), )+ }
            }

            /// Adds `other`'s lengths, column by column.
            #[allow(dead_code)]
            fn add(&mut self, other: &$Lens) {
                $( self.$col += other.$col; )+
            }
        }

        /// The sinks filling each column of a tree being built.
        struct $Sinks<'b> {
            $( $col: $crate::item_tree::block::Sink<'b, $T>, )+
            /// Last, so it is dropped after the sinks, which drop what they wrote into it.
            building: $crate::item_tree::block::Building<$Columns>,
        }

        impl $Sinks<'_> {
            fn new(lens: &$Lens) -> Self {
                let building =
                    $crate::item_tree::block::Building::<$Columns>::new(&[ $( lens.$col, )+ ]);
                $Sinks {
                    $(
                        // SAFETY: column `$Columns::$col` is `Shape::of::<$T>()` (`SHAPES`),
                        // this is its one sink, and `building`, declared after the sinks, is
                        // dropped or finished after them.
                        $col: unsafe { building.sink::<$T>($Columns::$col as usize) },
                    )+
                    building,
                }
            }

            fn finish(mut self) -> $Tree {
                $(
                    assert!(
                        self.$col.is_full(),
                        "the column `{}` of a `{}` left short: {} values written",
                        stringify!($col),
                        stringify!($Tree),
                        self.$col.len(),
                    );
                )+
                $( self.$col.disarm(); )+
                // SAFETY: every column is full of values its sink wrote and gave up above.
                $Tree { block: unsafe { self.building.finish() } }
            }
        }

        impl Default for $Tree {
            fn default() -> Self {
                $Tree::build($Lens::default(), |_| {})
            }
        }

        impl PartialEq for $Tree {
            fn eq(&self, other: &Self) -> bool {
                true $( && self.$col() == other.$col() )+
            }
        }

        impl Eq for $Tree {}

        impl ::std::fmt::Debug for $Tree {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.debug_struct(stringify!($Tree))
                    $( .field(stringify!($col), &self.$col()) )+
                    .finish()
            }
        }
    };
}

pub(super) use columns;

thread_local! {
    /// The references [`with_refs`] lends, empty between calls and keeping their capacity.
    static REFS: Cell<Scratch<NonNull<()>>> = const { Cell::new(Scratch::new()) };
}

/// The most references a kept [`REFS`] list has room for.
const REFS_KEPT: usize = 1 << 14;

/// A list of references, each valid for `'a`, lent by [`with_refs`].
pub(super) struct Refs<'s, 'a, T> {
    /// Each a `&'a T` ([`Refs::push`], the only writer).
    refs: &'s mut Vec<NonNull<()>>,
    values: PhantomData<&'a T>,
}

impl<'a, T> Refs<'_, 'a, T> {
    #[inline]
    pub(super) fn push(&mut self, value: &'a T) {
        self.refs.push(NonNull::from(value).cast());
    }

    pub(super) fn as_slice(&self) -> &[&'a T] {
        let refs = self.refs.as_ptr().cast::<&'a T>();
        // SAFETY: each element is a `&'a T` ([`Refs::push`]) kept as a `NonNull<()>`: both are
        // one non-null, aligned thin pointer, `T` being sized; the list lives while `self` is
        // borrowed.
        unsafe { slice::from_raw_parts(refs, self.refs.len()) }
    }
}

/// Lends the thread's list of references, empty, for `f`: no allocation once it has held as
/// many. A call inside `f` gets a list of its own.
pub(super) fn with_refs<'a, T, R>(f: impl FnOnce(&mut Refs<'_, 'a, T>) -> R) -> R {
    let mut scratch = REFS.replace(Scratch::new());
    let result = f(&mut Refs { refs: scratch.take(), values: PhantomData });
    if scratch.high_water() > REFS_KEPT {
        scratch.shrink(REFS_KEPT);
    }
    scratch.take();
    REFS.set(scratch);
    result
}

#[cfg(test)]
mod tests {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    /// A value that counts its drops.
    #[derive(Debug)]
    struct Counted {
        id: u32,
        drops: Arc<AtomicUsize>,
    }

    impl PartialEq for Counted {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    crate::item_tree::block::columns! {
        struct Tiny(TinyColumns, TinyLens, TinySinks) {
            bytes: u8,
            wide: u64,
            names: String,
            counted: Counted,
            pairs: [u32; 2],
            flags: bool,
        }
    }

    /// The reference a tree is checked against: each column a vector.
    #[derive(Default)]
    struct Reference {
        bytes: Vec<u8>,
        wide: Vec<u64>,
        names: Vec<String>,
        pairs: Vec<[u32; 2]>,
        flags: Vec<bool>,
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn lens_of(r: &Reference, counted: usize) -> TinyLens {
        TinyLens {
            bytes: r.bytes.len(),
            wide: r.wide.len(),
            names: r.names.len(),
            counted,
            pairs: r.pairs.len(),
            flags: r.flags.len(),
        }
    }

    fn aligned<T>(column: &[T]) -> bool {
        column.as_ptr() as usize % align_of::<T>() == 0
    }

    #[test]
    fn columns_read_back_what_was_written() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut made = 0;
        for _ in 0..200 {
            let mut r = Reference::default();
            // Some columns empty between full ones, so every alignment step is crossed.
            let len = |rng: &mut Rng| match rng.below(3) {
                0 => 0,
                _ => rng.below(40) as usize,
            };
            r.bytes = (0..len(&mut rng)).map(|i| i as u8).collect();
            r.wide = (0..len(&mut rng)).map(|i| (i as u64) << 40 | 7).collect();
            r.names = (0..len(&mut rng)).map(|i| format!("name{i}")).collect();
            r.pairs = (0..len(&mut rng)).map(|i| [i as u32, !(i as u32)]).collect();
            r.flags = (0..len(&mut rng)).map(|i| i % 3 == 0).collect();
            let counted = len(&mut rng);
            made += counted;
            let lens = lens_of(&r, counted);
            let mut names = r.names.clone();
            let tree = Tiny::build(lens, |s| {
                s.bytes.extend_from_slice(&r.bytes);
                s.wide.extend(r.wide.iter().copied());
                s.names.move_from(&mut names);
                s.counted
                    .extend((0..counted as u32).map(|id| Counted { id, drops: drops.clone() }));
                for &pair in &r.pairs {
                    s.pairs.push(pair);
                }
                s.flags.extend_from_slice(&r.flags);
            });
            assert!(names.is_empty());
            assert_eq!(TinyLens::of(&tree), lens);
            assert_eq!(tree.bytes(), &r.bytes[..]);
            assert_eq!(tree.wide(), &r.wide[..]);
            assert_eq!(tree.names(), &r.names[..]);
            assert_eq!(tree.pairs(), &r.pairs[..]);
            assert_eq!(tree.flags(), &r.flags[..]);
            assert!((tree.counted().iter().map(|it| it.id)).eq(0..counted as u32));
            assert!(aligned(tree.wide()) && aligned(tree.pairs()) && aligned(tree.names()));
            assert!(aligned(tree.counted()));
            drop(tree);
            assert_eq!(drops.load(Ordering::Relaxed), made, "every value dropped once");
        }
    }

    #[test]
    fn an_empty_tree_allocates_nothing() {
        let (tree, used) = blaze_alloc_count::measure(Tiny::default);
        assert_eq!(used.allocations, 0);
        assert_eq!(TinyLens::of(&tree), TinyLens::default());
        assert!(tree.names().is_empty() && tree.wide().is_empty() && tree.flags().is_empty());
        assert!(aligned(tree.wide()) && aligned(tree.names()));
        assert_eq!(tree, Tiny::default());
        let (_, used) = blaze_alloc_count::measure(|| drop(tree));
        assert_eq!(used.allocations, 0);
    }

    #[test]
    fn a_tree_is_one_allocation() {
        let wide = [1u64, 2, 3];
        let pairs = [[4u32, 5], [6, 7]];
        let lens = TinyLens { bytes: 5, wide: 3, pairs: 2, flags: 1, ..TinyLens::default() };
        let (tree, used) = blaze_alloc_count::measure(|| {
            Tiny::build(lens, |s| {
                s.bytes.extend(0..5);
                s.wide.extend_from_slice(&wide);
                s.pairs.extend_from_slice(&pairs);
                s.flags.push(true);
            })
        });
        assert_eq!(used.allocations, 1);
        assert_eq!(
            (tree.bytes(), tree.wide(), tree.pairs()),
            (&[0, 1, 2, 3, 4][..], &wide[..], &pairs[..])
        );
        let other = Tiny::build(lens, |s| {
            s.bytes.extend(0..5);
            s.wide.extend_from_slice(&wide);
            s.pairs.extend_from_slice(&pairs);
            s.flags.push(false);
        });
        assert_ne!(tree, other, "trees differing in one column");
    }

    fn counted(n: u32, drops: &Arc<AtomicUsize>) -> impl Iterator<Item = Counted> + '_ {
        (0..n).map(move |id| Counted { id, drops: drops.clone() })
    }

    #[test]
    fn a_fill_that_panics_drops_what_it_wrote() {
        let drops = Arc::new(AtomicUsize::new(0));
        let lens = TinyLens { counted: 5, names: 2, ..TinyLens::default() };
        let result = catch_unwind(AssertUnwindSafe(|| {
            Tiny::build(lens, |s| {
                s.counted.extend(counted(3, &drops));
                s.names.push("a".to_owned());
                panic!("a fill that stops");
            })
        }));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_column_left_short_stops_and_drops_what_it_wrote() {
        let drops = Arc::new(AtomicUsize::new(0));
        let lens = TinyLens { counted: 5, ..TinyLens::default() };
        let result = catch_unwind(AssertUnwindSafe(|| {
            Tiny::build(lens, |s| s.counted.extend(counted(2, &drops)))
        }));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_column_written_past_its_end_stops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let lens = TinyLens { counted: 2, ..TinyLens::default() };
        let result = catch_unwind(AssertUnwindSafe(|| {
            Tiny::build(lens, |s| s.counted.extend(counted(3, &drops)))
        }));
        assert!(result.is_err());
        assert_eq!(drops.load(Ordering::Relaxed), 3, "the two written and the one refused");
        let result = catch_unwind(AssertUnwindSafe(|| {
            Tiny::build(TinyLens { flags: 1, ..TinyLens::default() }, |s| {
                s.flags.extend_from_slice(&[true, false])
            })
        }));
        assert!(result.is_err());
    }

    #[test]
    fn a_failed_fill_drops_what_it_wrote() {
        let drops = Arc::new(AtomicUsize::new(0));
        let lens = TinyLens { counted: 4, ..TinyLens::default() };
        let result = Tiny::try_build(lens, |s| {
            s.counted.extend(counted(2, &drops));
            Err("refused")
        });
        assert_eq!(result.err(), Some("refused"));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        let tree = Tiny::try_build::<()>(lens, |s| {
            s.counted.extend(counted(4, &drops));
            Ok(())
        })
        .expect("a fill that succeeds");
        assert_eq!(tree.counted().len(), 4);
        drop(tree);
        assert_eq!(drops.load(Ordering::Relaxed), 6);
    }

    #[test]
    fn refs_are_the_references_pushed() {
        let values: Vec<u64> = (0..100).collect();
        let nested = super::with_refs(|refs| {
            refs.push(&values[1]);
            let inner = super::with_refs(|inner| {
                inner.push(&values[7]);
                inner.as_slice().iter().map(|it| **it).collect::<Vec<_>>()
            });
            (**refs.as_slice().first().expect("one pushed"), inner)
        });
        assert_eq!(nested, (1, vec![7]), "a call inside a call has a list of its own");
        let all = || {
            super::with_refs(|refs| {
                for value in &values {
                    refs.push(value);
                }
                let all = refs.as_slice();
                assert!(all.iter().zip(&values).all(|(a, b)| std::ptr::eq(*a, b)));
                all.len()
            })
        };
        assert_eq!(all(), 100);
        let (len, used) = blaze_alloc_count::measure(all);
        assert_eq!((len, used.allocations), (100, 0), "a warm list allocates nothing");
    }
}
```

### `crates/hir-def/src/item_tree.rs`

`mod block;`, the columns declared through `block::columns!` (`attr_index: AttrBlock` between
`attr_owners` and `attr_list`), the size assert, `attrs(id)` reading `attr_index()` and
`attr_owners()`, and `attr_index` replaced by `attr_blocks`, an iterator a build streams into
its sink (no `Box` of its own). Every `self.$column` read is the accessor.

```diff
--- a/crates/hir-def/src/item_tree.rs
+++ b/crates/hir-def/src/item_tree.rs
@@ -43,9 +43,11 @@
 //! (keyed by item, in [`ItemTree::attrs`]) sit in side tables. [`persist`] writes the columns as
 //! they are. Names, paths and visibilities are deduplicated within each top-level item, not
 //! across them, so a source file's tree is its items' trees concatenated: it is assembled from
-//! them, each lowered on its own ([`items`]).
+//! them, each lowered on its own ([`items`]). All the columns are one allocation: a header of
+//! where each ends, then the columns, placed by offsets (`block.rs`).
 
 mod attrs;
+mod block;
 mod items;
 mod lower;
 pub mod persist;
@@ -272,59 +274,65 @@
     ctx.lower_block(&block)
 }
 
-/// The item tree of a source file, as columns (the module's docs).
-#[derive(Debug, Default, Eq, PartialEq)]
-pub struct ItemTree {
-    top_level: Box<[ModItemId]>,
-    /// Per item: its kind, which payload column `data` indexes.
-    kinds: Box<[ItemKind]>,
-    /// Per item: its index in its kind's payload column.
-    data: Box<[u32]>,
-    /// Per item.
-    ast_ids: Box<[FileAstId<ast::Item>]>,
-    /// The items of inline modules and extern blocks, each list an [`IdRange`] of this.
-    children: Box<[ModItemId]>,
+block::columns! {
+    /// The item tree of a source file, as columns (the module's docs), all in one block: a salsa
+    /// memo has no lifetime group of its own, so it is one allocation of the global allocator,
+    /// made once a tree (none for a tree with no values).
+    pub struct ItemTree(ItemTreeColumn, ItemTreeLens, ItemTreeSinks) {
+        top_level: ModItemId,
+        /// Per item: its kind, which payload column `data` indexes.
+        kinds: ItemKind,
+        /// Per item: its index in its kind's payload column.
+        data: u32,
+        /// Per item.
+        ast_ids: FileAstId<ast::Item>,
+        /// The items of inline modules and extern blocks, each list an [`IdRange`] of this.
+        children: ModItemId,
 
-    consts: Box<[Const]>,
-    enums: Box<[Enum]>,
-    extern_blocks: Box<[ExternBlock]>,
-    extern_crates: Box<[ExternCrate]>,
-    functions: Box<[Function]>,
-    impls: Box<[Impl]>,
-    macro2s: Box<[Macro2]>,
-    macro_calls: Box<[MacroCall]>,
-    macro_rules: Box<[MacroRules]>,
-    mods: Box<[Mod]>,
-    statics: Box<[Static]>,
-    structs: Box<[Struct]>,
-    traits: Box<[Trait]>,
-    type_aliases: Box<[TypeAlias]>,
-    unions: Box<[Union]>,
-    uses: Box<[Use]>,
+        consts: Const,
+        enums: Enum,
+        extern_blocks: ExternBlock,
+        extern_crates: ExternCrate,
+        functions: Function,
+        impls: Impl,
+        macro2s: Macro2,
+        macro_calls: MacroCall,
+        macro_rules: MacroRules,
+        mods: Mod,
+        statics: Static,
+        structs: Struct,
+        traits: Trait,
+        type_aliases: TypeAlias,
+        unions: Union,
+        uses: Use,
 
-    /// Every `use` tree of the tree's `use` items, each child before its parent.
-    use_trees: Box<[UseTree]>,
-    /// The children of prefixed `use` trees, each list an [`IdRange`] of this.
-    use_tree_lists: Box<[UseTreeId]>,
-    /// The visibilities a struct's value-namespace constructor is the least of, each struct's an
-    /// [`IdRange`] of this.
-    ctor_vis: Box<[RawVisibilityId]>,
+        /// Every `use` tree of the tree's `use` items, each child before its parent.
+        use_trees: UseTree,
+        /// The children of prefixed `use` trees, each list an [`IdRange`] of this.
+        use_tree_lists: UseTreeId,
+        /// The visibilities a struct's value-namespace constructor is the least of, each
+        /// struct's an [`IdRange`] of this.
+        ctor_vis: RawVisibilityId,
 
-    names: Box<[Name]>,
-    paths: Box<[ModPath]>,
-    vis: Box<[RawVisibility]>,
+        names: Name,
+        paths: ModPath,
+        vis: RawVisibility,
 
-    /// Sorted by owner. The last, if its owner is [`ModItemId::TOP_OWNER`], is the top level's.
-    attr_owners: Box<[AttrOwner]>,
-    /// Which items have an owner in `attr_owners`, and where: made from it ([`attr_index`]),
-    /// so not written ([`persist`]).
-    attr_index: Box<[AttrBlock]>,
-    /// Every item's attributes, each item's an [`IdRange`] of this.
-    attrs: Box<[Attr]>,
-    /// The `cfg`s that disable items.
-    cfgs: Box<[CfgExpr]>,
+        /// Sorted by owner. The last, if its owner is [`ModItemId::TOP_OWNER`], is the top
+        /// level's.
+        attr_owners: AttrOwner,
+        /// Which items have an owner in `attr_owners`, and where: made from it ([`attr_blocks`]),
+        /// so not written ([`persist`]).
+        attr_index: AttrBlock,
+        /// Every item's attributes, each item's an [`IdRange`] of this.
+        attr_list: Attr,
+        /// The `cfg`s that disable items.
+        cfgs: CfgExpr,
+    }
 }
 
+const _: () = assert!(size_of::<ItemTree>() == size_of::<usize>());
+
 /// Bytes per item in the per-item columns (`kinds`, `data`, `ast_ids`); its kind's payload
 /// record (at most 16 bytes, [`Struct`] and [`Mod`]) is in its own column.
 pub const ITEM_BYTES: usize =
@@ -342,33 +350,40 @@
 const _: () = assert!(size_of::<AttrBlock>() == 16);
 
 /// The [`AttrBlock`]s of a tree of `items` items whose attribute owners are `owners`, sorted
-/// and in the tree (what `persist`'s `check` holds a decoded tree to).
-fn attr_index(owners: &[AttrOwner], items: usize) -> Box<[AttrBlock]> {
-    // alloc: the tree's column, once a tree at its length; none for a tree of no items
-    let mut blocks = vec![AttrBlock::default(); items.div_ceil(64)].into_boxed_slice();
-    for owner in owners.iter().filter(|owner| owner.owner != ModItemId::TOP_OWNER) {
-        let at = owner.owner.index as usize;
-        blocks[at / 64].bits |= 1 << (at % 64);
-    }
+/// and in the tree (what `persist`'s `check` holds a decoded tree to), in order: one pass, so
+/// they are written straight into the tree's block. Owners out of that order or past `items`
+/// (a buffer `check` refuses) make blocks that are wrong, never a panic.
+fn attr_blocks(owners: &[AttrOwner], items: usize) -> impl ExactSizeIterator<Item = AttrBlock> {
+    let mut owners = (owners.iter())
+        .filter(|owner| owner.owner != ModItemId::TOP_OWNER)
+        .map(|owner| owner.owner.index as usize)
+        .peekable();
     let mut before = 0;
-    for block in blocks.iter_mut() {
-        block.before = before;
-        before += block.bits.count_ones();
-    }
-    blocks
+    (0..items.div_ceil(64)).map(move |block| {
+        let mut bits = 0u64;
+        while let Some(&at) = owners.peek()
+            && at / 64 == block
+        {
+            bits |= 1 << (at % 64);
+            owners.next();
+        }
+        let it = AttrBlock { bits, before };
+        before += bits.count_ones();
+        it
+    })
 }
 
 impl ItemTree {
     /// Returns an iterator over all items located at the top level of the `HirFileId` this
     /// `ItemTree` was created from.
     pub(crate) fn top_level_items(&self) -> &[ModItemId] {
-        &self.top_level
+        self.top_level()
     }
 
     /// Returns the inner attributes of the source file.
     #[inline]
     pub(crate) fn top_level_attrs(&self) -> Option<AttrsOrCfgRef<'_>> {
-        self.attr_owners
+        self.attr_owners()
             .last()
             .filter(|owner| owner.owner == ModItemId::TOP_OWNER)
             .map(|owner| self.attrs_of(owner))
@@ -382,28 +397,28 @@
 
     #[inline]
     pub(crate) fn attrs(&self, of: ModItemId) -> Option<AttrsOrCfgRef<'_>> {
-        let block = self.attr_index.get(of.index as usize / 64)?;
+        let block = self.attr_index().get(of.index as usize / 64)?;
         let bit = 1u64 << (of.index % 64);
         if block.bits & bit == 0 {
             return None;
         }
         let at = block.before + (block.bits & (bit - 1)).count_ones();
-        Some(self.attrs_of(&self.attr_owners[at as usize]))
+        Some(self.attrs_of(&self.attr_owners()[at as usize]))
     }
 
     #[inline]
     fn attrs_of(&self, owner: &AttrOwner) -> AttrsOrCfgRef<'_> {
-        let attrs = Attrs::new(&self.attrs[owner.attrs.range()]);
+        let attrs = Attrs::new(&self.attr_list()[owner.attrs.range()]);
         match owner.cfg {
             AttrOwner::ENABLED => AttrsOrCfgRef::Enabled { attrs },
-            cfg => AttrsOrCfgRef::CfgDisabled(&self.cfgs[cfg as usize], attrs),
+            cfg => AttrsOrCfgRef::CfgDisabled(&self.cfgs()[cfg as usize], attrs),
         }
     }
 
     /// The items of an inline module's or an extern block's list.
     #[inline]
     pub(crate) fn items(&self, list: IdRange) -> &[ModItemId] {
-        &self.children[list.range()]
+        &self.children()[list.range()]
     }
 
     /// Whether `m` is inline, and its items if so.
@@ -418,7 +433,7 @@
     /// The visibility of a struct's constructor in the value namespace, if it has one.
     #[inline]
     pub(crate) fn value_ns_ctor(&self, s: &Struct) -> StructValueNsCtor<'_> {
-        let all = &self.ctor_vis[s.value_ns_ctor.range()];
+        let all = &self.ctor_vis()[s.value_ns_ctor.range()];
         match all {
             [] => StructValueNsCtor::NoValueNsCtor,
             [vis] => StructValueNsCtor::ValueNsCtorWithVis(vis),
@@ -447,7 +462,7 @@
 
     #[inline]
     pub fn use_tree(&self, id: UseTreeId) -> &UseTree {
-        &self.use_trees[id.raw as usize]
+        &self.use_trees()[id.raw as usize]
     }
 
     /// Returns a count of a few, expensive items.
@@ -455,11 +470,11 @@
     /// For more detail, see [`ItemTreeDataStats`].
     pub fn item_tree_stats(&self) -> ItemTreeDataStats {
         ItemTreeDataStats {
-            traits: self.traits.len(),
-            impls: self.impls.len(),
-            mods: self.mods.len(),
-            macro_calls: self.macro_calls.len(),
-            macro_rules: self.macro_rules.len(),
+            traits: self.traits().len(),
+            impls: self.impls().len(),
+            mods: self.mods().len(),
+            macro_calls: self.macro_calls().len(),
+            macro_rules: self.macro_rules().len(),
         }
     }
 
@@ -471,7 +486,7 @@
     fn is_empty(&self) -> bool {
         // We don't need to check the rest since if those are empty, everything is empty.
         // `attr_owners` might hold the top-level attrs so we need to check it separately.
-        self.top_level.is_empty() && self.attr_owners.is_empty()
+        self.top_level().is_empty() && self.attr_owners().is_empty()
     }
 }
 
@@ -479,7 +494,7 @@
     type Output = Name;
     #[inline]
     fn index(&self, id: NameId) -> &Name {
-        &self.names[id.raw as usize]
+        &self.names()[id.raw as usize]
     }
 }
 
@@ -487,7 +502,7 @@
     type Output = ModPath;
     #[inline]
     fn index(&self, id: PathId) -> &ModPath {
-        &self.paths[id.raw as usize]
+        &self.paths()[id.raw as usize]
     }
 }
 
@@ -560,7 +575,7 @@
 
     #[inline]
     pub(crate) fn ast_id(self, item_tree: &ItemTree) -> FileAstId<ast::Item> {
-        item_tree.ast_ids[self.index as usize]
+        item_tree.ast_ids()[self.index as usize]
     }
 }
 
@@ -593,16 +608,16 @@
             #[inline]
             pub(crate) fn index(&self, id: ModItemId) -> $mod_item<'_> {
                 let at = id.index as usize;
-                let (ast_id, data) = (self.ast_ids[at], self.data[at] as usize);
-                match self.kinds[at] {
-                    $( ItemKind::$typ => $mod_item::$typ(ast_id.downcast_unchecked(), &self.$column[data]), )+
+                let (ast_id, data) = (self.ast_ids()[at], self.data()[at] as usize);
+                match self.kinds()[at] {
+                    $( ItemKind::$typ => $mod_item::$typ(ast_id.downcast_unchecked(), &self.$column()[data]), )+
                 }
             }
 
             /// How long the payload column of `kind` is.
             fn column_len(&self, kind: ItemKind) -> usize {
                 match kind {
-                    $( ItemKind::$typ => self.$column.len(), )+
+                    $( ItemKind::$typ => self.$column().len(), )+
                 }
             }
         }
@@ -652,7 +667,7 @@
             RawVisibilityId::PRIV_EXPLICIT => &VIS_PRIV_EXPLICIT,
             RawVisibilityId::PUB => &VIS_PUB,
             RawVisibilityId::PUB_CRATE => &VIS_PUB_CRATE,
-            _ => &self.vis[index.raw as usize],
+            _ => &self.vis()[index.raw as usize],
         }
     }
 }
@@ -982,7 +997,7 @@
             UseTree::GLOB => UseTreeKind::Glob { path },
             _ => UseTreeKind::Prefixed {
                 prefix: path,
-                list: &tree.use_tree_lists[self.list.range()],
+                list: &tree.use_tree_lists()[self.list.range()],
             },
         }
     }
```

### `crates/hir-def/src/item_tree/lower.rs`

`finish` sums the builder's lengths into an `ItemTreeLens` and moves every column into the
block (`move_from` leaves each `Vec` empty with its capacity, as `column` did); `column` and
`Scoped::finish` go. Task 2 (scratch): `lower_use_tree` lowers into the worker's builder
(`SCRATCH`) and empties what it filled (`clear_use_trees`), not a fresh `TreeBuilder` a call.

```diff
--- a/crates/hir-def/src/item_tree/lower.rs
+++ b/crates/hir-def/src/item_tree/lower.rs
@@ -26,9 +26,9 @@
 
 use crate::item_tree::{
     AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, Impl,
-    ImportAlias, ItemKind, ItemTree, Macro2, MacroCall, MacroRules, Mod, ModItemId, ModPath,
-    NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias, Union, Use,
-    UseTree, UseTreeId, VisibilityExplicitness, attrs::lower_attrs_into, items::Marks,
+    ImportAlias, ItemKind, ItemTree, ItemTreeLens, Macro2, MacroCall, MacroRules, Mod, ModItemId,
+    ModPath, NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias,
+    Union, Use, UseTree, UseTreeId, VisibilityExplicitness, attrs::lower_attrs_into, items::Marks,
 };
 
 /// The index of the next element of an array `len` long, as a tree's `u32` ids count. The top
@@ -46,12 +46,6 @@
     at
 }
 
-/// Every column moved out into an exact-size one, `column` left empty with its capacity: one
-/// allocation per non-empty column, none for an empty one.
-fn column<T>(column: &mut Vec<T>) -> Box<[T]> {
-    column.drain(..).collect() // alloc: the tree's column, once a tree at its length
-}
-
 thread_local! {
     /// The worker's builder, empty between trees and keeping its columns' capacity, so a
     /// worker grows no column again once it has lowered a tree as big. A tree lowered while
@@ -151,9 +145,10 @@
         }
     }
 
-    fn finish(&mut self) -> Box<[T]> {
-        self.seal();
-        self.done.drain(..).collect() // alloc: the tree's table, once a tree at its length
+    /// Empties the table, keeping its capacity.
+    fn clear(&mut self) {
+        self.done.clear();
+        self.scope.clear();
     }
 }
 
@@ -206,47 +201,90 @@
         self.vis.seal();
     }
 
-    /// The tree built, the pending items its top level; leaves the builder empty, its capacity
-    /// kept.
+    /// The tree built, the pending items its top level, in one allocation (none for a tree with
+    /// no values); leaves the builder empty, its capacity kept.
     fn finish(&mut self) -> ItemTree {
         debug_assert!(self.pending_use_trees.is_empty() && self.ctor_scratch.is_empty());
         self.attr_owners.extend(self.top_attrs.take());
-        let attr_index = super::attr_index(&self.attr_owners, self.kinds.len());
-        ItemTree {
-            top_level: column(&mut self.pending_items),
-            kinds: column(&mut self.kinds),
-            data: column(&mut self.data),
-            ast_ids: column(&mut self.ast_ids),
-            children: column(&mut self.children),
-            consts: column(&mut self.consts),
-            enums: column(&mut self.enums),
-            extern_blocks: column(&mut self.extern_blocks),
-            extern_crates: column(&mut self.extern_crates),
-            functions: column(&mut self.functions),
-            impls: column(&mut self.impls),
-            macro2s: column(&mut self.macro2s),
-            macro_calls: column(&mut self.macro_calls),
-            macro_rules: column(&mut self.macro_rules),
-            mods: column(&mut self.mods),
-            statics: column(&mut self.statics),
-            structs: column(&mut self.structs),
-            traits: column(&mut self.traits),
-            type_aliases: column(&mut self.type_aliases),
-            unions: column(&mut self.unions),
-            uses: column(&mut self.uses),
-            use_trees: column(&mut self.use_trees),
-            use_tree_lists: column(&mut self.use_tree_lists),
-            ctor_vis: column(&mut self.ctor_vis),
-            names: self.names.finish(),
-            paths: self.paths.finish(),
-            vis: self.vis.finish(),
-            attr_owners: column(&mut self.attr_owners),
-            attr_index,
-            attrs: column(&mut self.attrs),
-            cfgs: column(&mut self.cfgs),
-        }
+        self.seal();
+        let lens = ItemTreeLens {
+            top_level: self.pending_items.len(),
+            kinds: self.kinds.len(),
+            data: self.data.len(),
+            ast_ids: self.ast_ids.len(),
+            children: self.children.len(),
+            consts: self.consts.len(),
+            enums: self.enums.len(),
+            extern_blocks: self.extern_blocks.len(),
+            extern_crates: self.extern_crates.len(),
+            functions: self.functions.len(),
+            impls: self.impls.len(),
+            macro2s: self.macro2s.len(),
+            macro_calls: self.macro_calls.len(),
+            macro_rules: self.macro_rules.len(),
+            mods: self.mods.len(),
+            statics: self.statics.len(),
+            structs: self.structs.len(),
+            traits: self.traits.len(),
+            type_aliases: self.type_aliases.len(),
+            unions: self.unions.len(),
+            uses: self.uses.len(),
+            use_trees: self.use_trees.len(),
+            use_tree_lists: self.use_tree_lists.len(),
+            ctor_vis: self.ctor_vis.len(),
+            names: self.names.done.len(),
+            paths: self.paths.done.len(),
+            vis: self.vis.done.len(),
+            attr_owners: self.attr_owners.len(),
+            attr_index: self.kinds.len().div_ceil(64),
+            attr_list: self.attrs.len(),
+            cfgs: self.cfgs.len(),
+        };
+        ItemTree::build(lens, |s| {
+            s.attr_index.extend(super::attr_blocks(&self.attr_owners, self.kinds.len()));
+            s.top_level.move_from(&mut self.pending_items);
+            s.kinds.move_from(&mut self.kinds);
+            s.data.move_from(&mut self.data);
+            s.ast_ids.move_from(&mut self.ast_ids);
+            s.children.move_from(&mut self.children);
+            s.consts.move_from(&mut self.consts);
+            s.enums.move_from(&mut self.enums);
+            s.extern_blocks.move_from(&mut self.extern_blocks);
+            s.extern_crates.move_from(&mut self.extern_crates);
+            s.functions.move_from(&mut self.functions);
+            s.impls.move_from(&mut self.impls);
+            s.macro2s.move_from(&mut self.macro2s);
+            s.macro_calls.move_from(&mut self.macro_calls);
+            s.macro_rules.move_from(&mut self.macro_rules);
+            s.mods.move_from(&mut self.mods);
+            s.statics.move_from(&mut self.statics);
+            s.structs.move_from(&mut self.structs);
+            s.traits.move_from(&mut self.traits);
+            s.type_aliases.move_from(&mut self.type_aliases);
+            s.unions.move_from(&mut self.unions);
+            s.uses.move_from(&mut self.uses);
+            s.use_trees.move_from(&mut self.use_trees);
+            s.use_tree_lists.move_from(&mut self.use_tree_lists);
+            s.ctor_vis.move_from(&mut self.ctor_vis);
+            s.names.move_from(&mut self.names.done);
+            s.paths.move_from(&mut self.paths.done);
+            s.vis.move_from(&mut self.vis.done);
+            s.attr_owners.move_from(&mut self.attr_owners);
+            s.attr_list.move_from(&mut self.attrs);
+            s.cfgs.move_from(&mut self.cfgs);
+        })
     }
 
+    /// Empties what lowering a `use` tree alone fills ([`lower_use_tree`]), keeping its
+    /// capacity: the rest of a builder between trees is empty already.
+    fn clear_use_trees(&mut self) {
+        self.pending_use_trees.clear();
+        self.use_trees.clear();
+        self.use_tree_lists.clear();
+        self.names.clear();
+        self.paths.clear();
+    }
+
     /// A new item of `kind`, its payload at `data` of its column.
     fn item(&mut self, kind: ItemKind, ast_id: FileAstId<ast::Item>, data: u32) -> ModItemId {
         let index = id(self.kinds.len());
@@ -998,10 +1036,15 @@
     // dyn: `ModPath::from_src`'s callback (hir-expand), once a path
     span_for_range: &mut dyn FnMut(::tt::TextRange) -> SyntaxContext,
 ) -> Option<Arena<ast::UseTree>> {
-    let mut scratch = TreeBuilder::default();
+    let mut scratch = SCRATCH.take();
     let mut mapping = Arena::new();
     let mut lowering = UseTreeLowering { db, mapping: Some(&mut mapping), tree: &mut scratch };
-    lowering.lower_use_tree(tree, span_for_range)?;
+    let lowered = lowering.lower_use_tree(tree, span_for_range);
+    scratch.clear_use_trees();
+    if scratch.kinds.capacity() <= SCRATCH_ITEMS {
+        SCRATCH.set(scratch);
+    }
+    lowered?;
     Some(mapping)
 }
 
```

### `crates/hir-def/src/item_tree/persist.rs`

`encode`, `check` and `assert_round_trip` read accessors and the `column` helper goes; `decode`
sizes the block from the buffer's column lengths and table counts (`Read::len`), then fills it
with `try_build`, validating kinds and macro calls inline as before (an error leaves the build,
which drops what it wrote), and checks the tree after. `attr_index` is built in the block from
the owners read (rule 3): `attr_blocks` never panics on owners not yet checked, it yields
exactly `kinds.div_ceil(64)` blocks, and `check` then refuses them as before.

Also a fix, needed for the file to lex at all (in HEAD since 152bfc9bf0): the fixture of
`a_tree_of_every_kind_round_trips` is `r#"..."#` and holds `r#"g"#`, which ends it; it is now
`r##"..."##`.

```diff
--- a/crates/hir-def/src/item_tree/persist.rs
+++ b/crates/hir-def/src/item_tree/persist.rs
@@ -23,9 +23,9 @@
 
 use super::{
     AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, FieldsShape, Function, IdRange,
-    Impl, ImportAlias, ItemKind, ItemTree, Macro2, MacroCall, MacroRules, Mod, ModItemId, NameId,
-    PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias, Union, Use,
-    UseTree, UseTreeId, VisibilityExplicitness,
+    Impl, ImportAlias, ItemKind, ItemTree, ItemTreeLens, Macro2, MacroCall, MacroRules, Mod,
+    ModItemId, NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias,
+    Union, Use, UseTree, UseTreeId, VisibilityExplicitness,
 };
 use crate::hir::persist::{Code, Column, Read, Words, WordsRec, syntax_context};
 
@@ -123,66 +123,135 @@
         self.len() == 0
     }
 
-    /// The owned tree, equal to the one written.
+    /// The owned tree, equal to the one written: its one block, sized from the buffer's column
+    /// lengths and table counts before a value is read.
     pub fn decode(self) -> Result<ItemTree, FormatError> {
         let (root, buffer) = (self.root, self.buffer);
         let invalid = |what| FormatError::Invalid { at: self.position(root), what };
-        if self.kinds.iter().any(|&kind| ItemKind::from_u8(kind).is_none()) {
-            return Err(invalid("an item kind out of range"));
+        macro_rules! follow {
+            ($($column:ident),+) => { $( let $column = root.$column.follow(buffer)?; )+ };
         }
-        let kinds: Box<[ItemKind]> = (self.kinds.iter())
-            .map(|&kind| ItemKind::ALL[kind as usize])
-            .collect(); // alloc: the tree's column, once a store read, the edge
-        let ast_ids = (self.ast_ids.iter())
-            .map(|&raw| FileAstId::from_erased_unchecked(ErasedFileAstId::from_raw(raw)))
-            .collect(); // alloc: the tree's column, once a store read, the edge
-        let calls = root.macro_calls.follow(buffer)?;
-        // alloc: the tree's column, once a store read, the edge
-        let mut macro_calls = Vec::with_capacity(calls.len());
-        for call in calls {
-            let expand_to = expand_to_of(call.expand_to)
-                .ok_or_else(|| invalid("a macro expansion kind out of range"))?;
-            let ctxt =
-                syntax_context(call.ctxt).ok_or_else(|| invalid("a syntax context of u32::MAX"))?;
-            macro_calls.push(MacroCall { path: call.path, ctxt, expand_to });
-        }
-        let macro_calls = macro_calls.into_boxed_slice();
-        let words = Read::new(&root.words, buffer);
-        let mut tree = ItemTree {
-            top_level: self.top_level.into(),
-            kinds,
-            data: column(&root.data, buffer)?,
-            ast_ids,
-            children: self.children.into(),
-            consts: column(&root.consts, buffer)?,
-            enums: column(&root.enums, buffer)?,
-            extern_blocks: column(&root.extern_blocks, buffer)?,
-            extern_crates: column(&root.extern_crates, buffer)?,
-            functions: column(&root.functions, buffer)?,
-            impls: column(&root.impls, buffer)?,
-            macro2s: column(&root.macro2s, buffer)?,
+        follow!(
+            data,
+            consts,
+            enums,
+            extern_blocks,
+            extern_crates,
+            functions,
+            impls,
+            macro2s,
             macro_calls,
-            macro_rules: column(&root.macro_rules, buffer)?,
-            mods: column(&root.mods, buffer)?,
-            statics: column(&root.statics, buffer)?,
-            structs: column(&root.structs, buffer)?,
-            traits: column(&root.traits, buffer)?,
-            type_aliases: column(&root.type_aliases, buffer)?,
-            unions: column(&root.unions, buffer)?,
-            uses: column(&root.uses, buffer)?,
-            use_trees: column(&root.use_trees, buffer)?,
-            use_tree_lists: column(&root.use_tree_lists, buffer)?,
-            ctor_vis: column(&root.ctor_vis, buffer)?,
-            names: words.from(root.names_at).get()?,
-            paths: words.from(root.paths_at).get()?,
-            vis: words.from(root.vis_at).get()?,
-            attr_owners: column(&root.attr_owners, buffer)?,
-            attr_index: Box::default(),
-            attrs: words.from(root.attrs_at).get()?,
-            cfgs: words.from(root.cfgs_at).get()?,
+            macro_rules,
+            mods,
+            statics,
+            structs,
+            traits,
+            type_aliases,
+            unions,
+            uses,
+            use_trees,
+            use_tree_lists,
+            ctor_vis,
+            attr_owners
+        );
+        let words = Read::new(&root.words, buffer);
+        let mut names = words.from(root.names_at);
+        let mut paths = words.from(root.paths_at);
+        let mut vis = words.from(root.vis_at);
+        let mut attrs = words.from(root.attrs_at);
+        let mut cfgs = words.from(root.cfgs_at);
+        let lens = ItemTreeLens {
+            top_level: self.top_level.len(),
+            kinds: self.kinds.len(),
+            data: data.len(),
+            ast_ids: self.ast_ids.len(),
+            children: self.children.len(),
+            consts: consts.len(),
+            enums: enums.len(),
+            extern_blocks: extern_blocks.len(),
+            extern_crates: extern_crates.len(),
+            functions: functions.len(),
+            impls: impls.len(),
+            macro2s: macro2s.len(),
+            macro_calls: macro_calls.len(),
+            macro_rules: macro_rules.len(),
+            mods: mods.len(),
+            statics: statics.len(),
+            structs: structs.len(),
+            traits: traits.len(),
+            type_aliases: type_aliases.len(),
+            unions: unions.len(),
+            uses: uses.len(),
+            use_trees: use_trees.len(),
+            use_tree_lists: use_tree_lists.len(),
+            ctor_vis: ctor_vis.len(),
+            names: names.len()?,
+            paths: paths.len()?,
+            vis: vis.len()?,
+            attr_owners: attr_owners.len(),
+            attr_index: self.kinds.len().div_ceil(64),
+            attr_list: attrs.len()?,
+            cfgs: cfgs.len()?,
         };
+        // alloc: the tree's block, once a store read, the edge
+        let tree = ItemTree::try_build(lens, |s| {
+            s.top_level.extend_from_slice(self.top_level);
+            for &kind in self.kinds {
+                let kind =
+                    ItemKind::from_u8(kind).ok_or_else(|| invalid("an item kind out of range"))?;
+                s.kinds.push(kind);
+            }
+            s.data.extend_from_slice(data);
+            s.ast_ids.extend(
+                (self.ast_ids.iter())
+                    .map(|&raw| FileAstId::from_erased_unchecked(ErasedFileAstId::from_raw(raw))),
+            );
+            s.children.extend_from_slice(self.children);
+            s.consts.extend_from_slice(consts);
+            s.enums.extend_from_slice(enums);
+            s.extern_blocks.extend_from_slice(extern_blocks);
+            s.extern_crates.extend_from_slice(extern_crates);
+            s.functions.extend_from_slice(functions);
+            s.impls.extend_from_slice(impls);
+            s.macro2s.extend_from_slice(macro2s);
+            for call in macro_calls {
+                let expand_to = expand_to_of(call.expand_to)
+                    .ok_or_else(|| invalid("a macro expansion kind out of range"))?;
+                let ctxt = syntax_context(call.ctxt)
+                    .ok_or_else(|| invalid("a syntax context of u32::MAX"))?;
+                s.macro_calls.push(MacroCall { path: call.path, ctxt, expand_to });
+            }
+            s.macro_rules.extend_from_slice(macro_rules);
+            s.mods.extend_from_slice(mods);
+            s.statics.extend_from_slice(statics);
+            s.structs.extend_from_slice(structs);
+            s.traits.extend_from_slice(traits);
+            s.type_aliases.extend_from_slice(type_aliases);
+            s.unions.extend_from_slice(unions);
+            s.uses.extend_from_slice(uses);
+            s.use_trees.extend_from_slice(use_trees);
+            s.use_tree_lists.extend_from_slice(use_tree_lists);
+            s.ctor_vis.extend_from_slice(ctor_vis);
+            for _ in 0..lens.names {
+                s.names.push(names.get()?);
+            }
+            for _ in 0..lens.paths {
+                s.paths.push(paths.get()?);
+            }
+            for _ in 0..lens.vis {
+                s.vis.push(vis.get()?);
+            }
+            s.attr_owners.extend_from_slice(attr_owners);
+            s.attr_index.extend(super::attr_blocks(attr_owners, self.kinds.len()));
+            for _ in 0..lens.attr_list {
+                s.attr_list.push(attrs.get()?);
+            }
+            for _ in 0..lens.cfgs {
+                s.cfgs.push(cfgs.get()?);
+            }
+            Ok(())
+        })?;
         check(&tree).map_err(invalid)?;
-        tree.attr_index = super::attr_index(&tree.attr_owners, tree.kinds.len());
         Ok(tree)
     }
 
@@ -191,85 +260,80 @@
     }
 }
 
-/// The records of a column, copied out of the buffer.
-fn column<T: Record>(slice: &Slice<T>, buffer: &[u8]) -> Result<Box<[T]>, FormatError> {
-    Ok(slice.follow(buffer)?.into())
-}
-
 /// Whether every id the tree's records hold names something the tree has, every list is within
 /// its array and holds only what comes before its holder, and the attributes are in the order
 /// [`ItemTree::attrs`] searches them in: what indexing the tree relies on.
 fn check(tree: &ItemTree) -> Result<(), &'static str> {
-    let items = tree.kinds.len();
-    if tree.data.len() != items || tree.ast_ids.len() != items {
+    let items = tree.kinds().len();
+    if tree.data().len() != items || tree.ast_ids().len() != items {
         return Err("per-item columns of different lengths");
     }
-    let vis = |id: RawVisibilityId| id.is_builtin() || (id.raw as usize) < tree.vis.len();
-    let name = |id: NameId| (id.raw as usize) < tree.names.len();
-    let path = |id: PathId| (id.raw as usize) < tree.paths.len();
+    let vis = |id: RawVisibilityId| id.is_builtin() || (id.raw as usize) < tree.vis().len();
+    let name = |id: NameId| (id.raw as usize) < tree.names().len();
+    let path = |id: PathId| (id.raw as usize) < tree.paths().len();
     let path_or_none = |id: PathId| id == PathId::NONE || path(id);
     let alias =
         |id: AliasId| matches!(id, AliasId::NONE | AliasId::UNDERSCORE) || name(NameId { raw: id.raw });
     let named = |name_id: NameId, vis_id: RawVisibilityId| name(name_id) && vis(vis_id);
     // A list of items, each before the item `holder` that holds the list.
     let list = |range: IdRange, holder: usize| {
-        range.within(tree.children.len())
-            && tree.children[range.range()].iter().all(|child| (child.index as usize) < holder)
+        range.within(tree.children().len())
+            && tree.children()[range.range()].iter().all(|child| (child.index as usize) < holder)
     };
 
-    if !tree.top_level.iter().all(|id| (id.index as usize) < items) {
+    if !tree.top_level().iter().all(|id| (id.index as usize) < items) {
         return Err("a top-level item that is not in the tree");
     }
     for at in 0..items {
-        let (kind, data) = (tree.kinds[at], tree.data[at] as usize);
+        let (kind, data) = (tree.kinds()[at], tree.data()[at] as usize);
         if data >= tree.column_len(kind) {
             return Err("an item past the column of its kind");
         }
         let ok = match kind {
             ItemKind::Const => {
-                let it = &tree.consts[data];
+                let it = &tree.consts()[data];
                 (it.name == NameId::NONE || name(it.name)) && vis(it.visibility)
             }
-            ItemKind::Enum => named(tree.enums[data].name, tree.enums[data].visibility),
-            ItemKind::ExternBlock => list(tree.extern_blocks[data].children, at),
+            ItemKind::Enum => named(tree.enums()[data].name, tree.enums()[data].visibility),
+            ItemKind::ExternBlock => list(tree.extern_blocks()[data].children, at),
             ItemKind::ExternCrate => {
-                let it = &tree.extern_crates[data];
+                let it = &tree.extern_crates()[data];
                 named(it.name, it.visibility) && alias(it.alias)
             }
             ItemKind::Function => {
-                named(tree.functions[data].name, tree.functions[data].visibility)
+                named(tree.functions()[data].name, tree.functions()[data].visibility)
             }
             ItemKind::Impl => true,
-            ItemKind::Macro2 => named(tree.macro2s[data].name, tree.macro2s[data].visibility),
-            ItemKind::MacroCall => path(tree.macro_calls[data].path),
-            ItemKind::MacroRules => name(tree.macro_rules[data].name),
+            ItemKind::Macro2 => named(tree.macro2s()[data].name, tree.macro2s()[data].visibility),
+            ItemKind::MacroCall => path(tree.macro_calls()[data].path),
+            ItemKind::MacroRules => name(tree.macro_rules()[data].name),
             ItemKind::Mod => {
-                let it = &tree.mods[data];
+                let it = &tree.mods()[data];
                 named(it.name, it.visibility) && (it.items == IdRange::NONE || list(it.items, at))
             }
-            ItemKind::Static => named(tree.statics[data].name, tree.statics[data].visibility),
+            ItemKind::Static => named(tree.statics()[data].name, tree.statics()[data].visibility),
             ItemKind::Struct => {
-                let it = &tree.structs[data];
-                named(it.name, it.visibility) && it.value_ns_ctor.within(tree.ctor_vis.len())
+                let it = &tree.structs()[data];
+                named(it.name, it.visibility) && it.value_ns_ctor.within(tree.ctor_vis().len())
             }
-            ItemKind::Trait => named(tree.traits[data].name, tree.traits[data].visibility),
+            ItemKind::Trait => named(tree.traits()[data].name, tree.traits()[data].visibility),
             ItemKind::TypeAlias => {
-                named(tree.type_aliases[data].name, tree.type_aliases[data].visibility)
+                named(tree.type_aliases()[data].name, tree.type_aliases()[data].visibility)
             }
-            ItemKind::Union => named(tree.unions[data].name, tree.unions[data].visibility),
+            ItemKind::Union => named(tree.unions()[data].name, tree.unions()[data].visibility),
             ItemKind::Use => {
-                let it = &tree.uses[data];
-                vis(it.visibility) && (it.use_tree.raw as usize) < tree.use_trees.len()
+                let it = &tree.uses()[data];
+                vis(it.visibility) && (it.use_tree.raw as usize) < tree.use_trees().len()
             }
         };
         if !ok {
             return Err("an item naming something that is not in the tree");
         }
     }
-    if !tree.ctor_vis.iter().all(|&id| vis(id)) {
+    if !tree.ctor_vis().iter().all(|&id| vis(id)) {
         return Err("a constructor visibility that is not in the tree");
     }
-    for (at, it) in tree.use_trees.iter().enumerate() {
+    for (at, it) in tree.use_trees().iter().enumerate() {
         let ok = match it.kind {
             UseTree::SINGLE => path(it.path) && alias(it.alias) && it.list == IdRange::EMPTY,
             UseTree::GLOB => {
@@ -278,8 +342,8 @@
             UseTree::PREFIXED => {
                 path_or_none(it.path)
                     && it.alias == AliasId::NONE
-                    && it.list.within(tree.use_tree_lists.len())
-                    && (tree.use_tree_lists[it.list.range()].iter())
+                    && it.list.within(tree.use_tree_lists().len())
+                    && (tree.use_tree_lists()[it.list.range()].iter())
                         .all(|child| (child.raw as usize) < at)
             }
             _ => false,
@@ -288,18 +352,18 @@
             return Err("a use tree that is not one, or not before the tree holding it");
         }
     }
-    for owner in &tree.attr_owners {
+    for owner in tree.attr_owners() {
         if owner.owner != ModItemId::TOP_OWNER && owner.owner.index as usize >= items {
             return Err("attributes of an item that is not in the tree");
         }
-        if !owner.attrs.within(tree.attrs.len()) {
+        if !owner.attrs.within(tree.attr_list().len()) {
             return Err("attributes past the tree's");
         }
-        if owner.cfg != AttrOwner::ENABLED && owner.cfg as usize >= tree.cfgs.len() {
+        if owner.cfg != AttrOwner::ENABLED && owner.cfg as usize >= tree.cfgs().len() {
             return Err("a `cfg` that is not in the tree");
         }
     }
-    if !tree.attr_owners.windows(2).all(|pair| pair[0].owner < pair[1].owner) {
+    if !tree.attr_owners().windows(2).all(|pair| pair[0].owner < pair[1].owner) {
         return Err("attributes not in the order of their items");
     }
     Ok(())
@@ -324,21 +388,21 @@
     fn write(&self, out: &mut Writer) -> u64 {
         let mut w = Words::new();
         let names_at = w.at();
-        w.seq(self.names.iter());
+        w.seq(self.names().iter());
         let paths_at = w.at();
-        w.seq(self.paths.iter());
+        w.seq(self.paths().iter());
         let vis_at = w.at();
-        w.seq(self.vis.iter());
+        w.seq(self.vis().iter());
         let attrs_at = w.at();
-        w.seq(self.attrs.iter());
+        w.seq(self.attr_list().iter());
         let cfgs_at = w.at();
-        w.seq(self.cfgs.iter());
+        w.seq(self.cfgs().iter());
 
         // alloc: once a store write, the edge
-        let kinds: Vec<u8> = self.kinds.iter().map(|&kind| kind as u8).collect();
+        let kinds: Vec<u8> = self.kinds().iter().map(|&kind| kind as u8).collect();
         // alloc: once a store write, the edge
-        let ast_ids: Vec<u32> = self.ast_ids.iter().map(|id| id.erase().into_raw()).collect();
-        let macro_calls: Vec<MacroCallRec> = (self.macro_calls.iter())
+        let ast_ids: Vec<u32> = self.ast_ids().iter().map(|id| id.erase().into_raw()).collect();
+        let macro_calls: Vec<MacroCallRec> = (self.macro_calls().iter())
             .map(|call| MacroCallRec {
                 path: call.path,
                 ctxt: call.ctxt.into_u32(),
@@ -347,31 +411,31 @@
             .collect(); // alloc: once a store write, the edge
 
         let placed = w.place(out);
-        let top_level = Column::push(out, &self.top_level);
+        let top_level = Column::push(out, self.top_level());
         let kinds = Column::push(out, &kinds);
-        let data = Column::push(out, &self.data);
+        let data = Column::push(out, self.data());
         let ast_ids = Column::push(out, &ast_ids);
-        let children = Column::push(out, &self.children);
-        let consts = Column::push(out, &self.consts);
-        let enums = Column::push(out, &self.enums);
-        let extern_blocks = Column::push(out, &self.extern_blocks);
-        let extern_crates = Column::push(out, &self.extern_crates);
-        let functions = Column::push(out, &self.functions);
-        let impls = Column::push(out, &self.impls);
-        let macro2s = Column::push(out, &self.macro2s);
+        let children = Column::push(out, self.children());
+        let consts = Column::push(out, self.consts());
+        let enums = Column::push(out, self.enums());
+        let extern_blocks = Column::push(out, self.extern_blocks());
+        let extern_crates = Column::push(out, self.extern_crates());
+        let functions = Column::push(out, self.functions());
+        let impls = Column::push(out, self.impls());
+        let macro2s = Column::push(out, self.macro2s());
         let macro_calls = Column::push(out, &macro_calls);
-        let macro_rules = Column::push(out, &self.macro_rules);
-        let mods = Column::push(out, &self.mods);
-        let statics = Column::push(out, &self.statics);
-        let structs = Column::push(out, &self.structs);
-        let traits = Column::push(out, &self.traits);
-        let type_aliases = Column::push(out, &self.type_aliases);
-        let unions = Column::push(out, &self.unions);
-        let uses = Column::push(out, &self.uses);
-        let use_trees = Column::push(out, &self.use_trees);
-        let use_tree_lists = Column::push(out, &self.use_tree_lists);
-        let ctor_vis = Column::push(out, &self.ctor_vis);
-        let attr_owners = Column::push(out, &self.attr_owners);
+        let macro_rules = Column::push(out, self.macro_rules());
+        let mods = Column::push(out, self.mods());
+        let statics = Column::push(out, self.statics());
+        let structs = Column::push(out, self.structs());
+        let traits = Column::push(out, self.traits());
+        let type_aliases = Column::push(out, self.type_aliases());
+        let unions = Column::push(out, self.unions());
+        let uses = Column::push(out, self.uses());
+        let use_trees = Column::push(out, self.use_trees());
+        let use_tree_lists = Column::push(out, self.use_tree_lists());
+        let ctor_vis = Column::push(out, self.ctor_vis());
+        let attr_owners = Column::push(out, self.attr_owners());
 
         let at = out.next::<Root>();
         let field = |offset: usize| at + offset as u64;
@@ -846,26 +910,26 @@
     let bytes = blaze_format::to_bytes(tree);
     let buffer = blaze_format::Aligned::copy_from(&bytes);
     let read = ItemTree::read(&buffer).expect("an item tree just written");
-    assert_eq!(read.top_level().collect::<Vec<_>>(), tree.top_level.to_vec());
-    assert_eq!(read.len(), tree.kinds.len());
-    for index in 0..tree.kinds.len() as u32 {
+    assert_eq!(read.top_level().collect::<Vec<_>>(), tree.top_level().to_vec());
+    assert_eq!(read.len(), tree.kinds().len());
+    for index in 0..tree.kinds().len() as u32 {
         let item = ModItemId { index };
-        assert_eq!(read.kind(item), Some(tree.kinds[index as usize]));
+        assert_eq!(read.kind(item), Some(tree.kinds()[index as usize]));
         assert_eq!(read.ast_id(item), Some(item.ast_id(tree).erase()));
     }
-    let past = ModItemId { index: tree.kinds.len() as u32 };
+    let past = ModItemId { index: tree.kinds().len() as u32 };
     assert_eq!((read.kind(past), read.ast_id(past)), (None, None));
-    for (&kind, &data) in tree.kinds.iter().zip(&tree.data) {
+    for (&kind, &data) in tree.kinds().iter().zip(tree.data()) {
         let list = match kind {
-            ItemKind::Mod if tree.mods[data as usize].items != IdRange::NONE => {
-                tree.mods[data as usize].items
+            ItemKind::Mod if tree.mods()[data as usize].items != IdRange::NONE => {
+                tree.mods()[data as usize].items
             }
-            ItemKind::ExternBlock => tree.extern_blocks[data as usize].children,
+            ItemKind::ExternBlock => tree.extern_blocks()[data as usize].children,
             _ => continue,
         };
         assert_eq!(read.children(list), Some(tree.items(list)));
     }
-    assert_eq!(read.children(IdRange { start: 0, len: tree.children.len() as u32 + 1 }), None);
+    assert_eq!(read.children(IdRange { start: 0, len: tree.children().len() as u32 + 1 }), None);
     let decoded = read.decode().expect("an item tree just written");
     assert_eq!(decoded, *tree);
     assert_eq!(blaze_format::to_bytes(&decoded), bytes, "an item tree written twice differs");
@@ -891,7 +955,7 @@
     #[test]
     fn every_item_kind_round_trips() {
         round_trip_fixture(
-            r#"
+            r##"
 //! crate docs
 #![no_std]
 #![feature(decl_macro)]
@@ -962,7 +1026,7 @@
 #[cfg(any(not(all()), target_os = "linux"))]
 #[after]
 struct DisabledLater;
-"#,
+"##,
         );
     }
 
```

### `crates/hir-def/src/item_tree/tests.rs`

Accessors for the reads; the column budgets tightened to the block:
`a_warm_builder_lowers_a_file_in_one_allocation_a_column` becomes
`a_warm_builder_lowers_a_file_in_one_allocation` (at most 1), and
`decoding_a_tree_allocates_once_a_column` becomes
`decoding_a_tree_allocates_its_block_and_its_paths`: at most 1 + one a `ModPath` with segments
decoded (paths, `pub(in ..)` visibilities, attribute paths), as `Code for ModPath` in
`hir::persist` collects its names into a `Vec` (not this lane's path; with that fixed it is 1).
`non_empty_columns` goes (unused). Every other test is kept as is.

```diff
--- a/crates/hir-def/src/item_tree/tests.rs
+++ b/crates/hir-def/src/item_tree/tests.rs
@@ -6,7 +6,7 @@
 use test_fixture::WithFixture;
 
 use crate::{
-    item_tree::{ItemTree, ModItemKind},
+    item_tree::{ItemTree, ModItemKind, RawVisibility},
     test_db::TestDB,
 };
 
@@ -416,7 +416,7 @@
     db.set_file_text(file_id.file_id(&db), &EDITED.replace("pub fn f(", "pub fn renamed("));
     let (after, _) = owned_tree(&db, file);
 
-    let f = (before.top_level.iter().copied())
+    let f = (before.top_level().iter().copied())
         .find(|&item| {
             matches!(before.index(item), ModItemKind::Function(_, it) if before[it.name].as_str() == "f")
         })
@@ -424,20 +424,22 @@
     let ModItemKind::Function(_, it) = after.index(f) else { panic!("`f`'s row is a fn") };
     assert_eq!(after[it.name].as_str(), "renamed");
 
-    assert_eq!(after.top_level, before.top_level);
-    assert_eq!(after.kinds, before.kinds);
-    assert_eq!(after.data, before.data);
-    assert_eq!(after.functions, before.functions);
-    assert_eq!(after.children, before.children);
-    assert_eq!((&after.structs, &after.mods, &after.uses), (&before.structs, &before.mods, &before.uses));
-    for (at, (old, new)) in before.ast_ids.iter().zip(&after.ast_ids).enumerate() {
+    assert_eq!(after.top_level(), before.top_level());
+    assert_eq!(after.kinds(), before.kinds());
+    assert_eq!(after.data(), before.data());
+    assert_eq!(after.functions(), before.functions());
+    assert_eq!(after.children(), before.children());
+    assert_eq!(
+        (after.structs(), after.mods(), after.uses()),
+        (before.structs(), before.mods(), before.uses())
+    );
+    for (at, (old, new)) in before.ast_ids().iter().zip(after.ast_ids()).enumerate() {
         if at != f.index as usize {
             assert_eq!(old, new, "the ast id of item {at}, not the one edited");
         }
     }
-    let changed: Vec<usize> = (0..before.names.len())
-        .filter(|&at| before.names[at] != after.names[at])
-        .collect();
+    let changed: Vec<usize> =
+        (0..before.names().len()).filter(|&at| before.names()[at] != after.names()[at]).collect();
     assert_eq!(changed, [it.name.raw as usize], "only the edited item's name");
 }
 
@@ -513,11 +515,11 @@
         .collect();
     let (db, file_id) = TestDB::with_single_file(&format!("#![c]\n{items}"));
     let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
-    assert!(tree.kinds.len() > 128, "more than two blocks of items");
-    for index in 0..tree.kinds.len() as u32 {
+    assert!(tree.kinds().len() > 128, "more than two blocks of items");
+    for index in 0..tree.kinds().len() as u32 {
         let id = ModItemId::new(index);
-        let searched = (tree.attr_owners.binary_search_by_key(&id, |owner| owner.owner).ok())
-            .map(|at| tree.attrs_of(&tree.attr_owners[at]));
+        let searched = (tree.attr_owners().binary_search_by_key(&id, |owner| owner.owner).ok())
+            .map(|at| tree.attrs_of(&tree.attr_owners()[at]));
         match (tree.attrs(id), searched) {
             (None, None) => {}
             (Some(found), Some(searched)) => assert!(same(found, searched), "item {index}"),
@@ -551,43 +553,16 @@
 pub fn f() {}
 "#;
 
-/// The columns of `tree` that hold anything: lowering on a warm builder and decoding allocate
-/// once for each.
-fn non_empty_columns(tree: &ItemTree) -> u64 {
-    let lens = [
-        tree.top_level.len(),
-        tree.kinds.len(),
-        tree.data.len(),
-        tree.ast_ids.len(),
-        tree.children.len(),
-        tree.consts.len(),
-        tree.enums.len(),
-        tree.extern_blocks.len(),
-        tree.extern_crates.len(),
-        tree.functions.len(),
-        tree.impls.len(),
-        tree.macro2s.len(),
-        tree.macro_calls.len(),
-        tree.macro_rules.len(),
-        tree.mods.len(),
-        tree.statics.len(),
-        tree.structs.len(),
-        tree.traits.len(),
-        tree.type_aliases.len(),
-        tree.unions.len(),
-        tree.uses.len(),
-        tree.use_trees.len(),
-        tree.use_tree_lists.len(),
-        tree.ctor_vis.len(),
-        tree.names.len(),
-        tree.paths.len(),
-        tree.vis.len(),
-        tree.attr_owners.len(),
-        tree.attr_index.len(),
-        tree.attrs.len(),
-        tree.cfgs.len(),
-    ];
-    lens.iter().filter(|&&len| len > 0).count() as u64
+/// The paths with segments a decode of `tree` reads, of its paths, visibilities and attributes:
+/// each a `Vec` of its names (`Code for ModPath` in `crate::hir::persist`).
+fn decoded_paths(tree: &ItemTree) -> u64 {
+    let vis = tree.vis().iter().filter_map(|vis| match vis {
+        RawVisibility::Module(path, _) => Some(path),
+        _ => None,
+    });
+    let attrs = tree.attr_list().iter().map(|attr| &attr.path);
+    (tree.paths().iter().chain(vis).chain(attrs)).filter(|path| !path.segments().is_empty()).count()
+        as u64
 }
 
 /// Reads every item under `items` through the accessors the def map collector uses; returns how
@@ -632,7 +607,7 @@
 fn reading_a_tree_allocates_nothing() {
     let (db, file_id) = TestDB::with_single_file(EVERY_KIND);
     let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
-    assert_eq!(read_all(tree, tree.top_level_items()), tree.kinds.len(), "every item read");
+    assert_eq!(read_all(tree, tree.top_level_items()), tree.kinds().len(), "every item read");
     blaze_alloc_count::assert_at_most(0, "reading every item of a tree", || {
         std::hint::black_box(read_all(tree, tree.top_level_items()));
         std::hint::black_box(tree.top_level_attrs());
@@ -660,27 +635,26 @@
 }
 
 #[test]
-fn a_warm_builder_lowers_a_file_in_one_allocation_a_column() {
+fn a_warm_builder_lowers_a_file_in_one_allocation() {
     let (db, file_id) = TestDB::with_single_file(EVERY_KIND);
     let (file, krate) = (HirFileId::from(file_id), db.test_crate());
     let cold = super::lower_whole(&db, file, krate);
-    let columns = non_empty_columns(&cold);
-    let warm = blaze_alloc_count::assert_at_most(columns, "lowering on a warm builder", || {
+    let warm = blaze_alloc_count::assert_at_most(1, "lowering on a warm builder", || {
         super::lower_whole(&db, file, krate)
     });
     assert_eq!(warm, cold);
 }
 
 #[test]
-fn decoding_a_tree_allocates_once_a_column() {
+fn decoding_a_tree_allocates_its_block_and_its_paths() {
     let (db, file_id) = TestDB::with_single_file(EVERY_KIND);
     let tree = crate::file_item_tree(&db, file_id.into(), db.test_crate());
     let buffer = blaze_format::Aligned::copy_from(&blaze_format::to_bytes(tree));
     let read = ItemTree::read(&buffer).expect("a tree written");
     assert_eq!(read.decode().expect("a tree written"), *tree);
     let decoded = blaze_alloc_count::assert_at_most(
-        non_empty_columns(tree),
-        "decoding a tree whose names are interned",
+        1 + decoded_paths(tree),
+        "decoding a tree whose names and paths are interned",
         || read.decode().expect("a tree written"),
     );
     assert_eq!(decoded, *tree);
```

### `crates/hir-def/src/item_tree/items.rs`

As of 20:45, before F: `assemble` sums the parts' lengths and fills one block (rules 2 to 4),
and `item_sources` kept every item's syntax, ids and anchors in one shared block
(`FileSyntax`, one allocation a run of the query), its spans read in place from the anchors
(`Spans::Item { file, anchors, item }`, the same spans as `RealSpanMap::span_for_range`, tested
against it). F's rewrite drops `item_sources`, `item_fragment` and per-item lowering, so only the
`assemble` part carries over: port it by rules 2 to 4. The rest is withdrawn if F lands as it
is going; it is here so the per-item design's one-block form is on record.

```diff
--- a/crates/hir-def/src/item_tree/items.rs
+++ b/crates/hir-def/src/item_tree/items.rs
@@ -8,44 +8,56 @@
 //! [`super::lower_whole`] lowers in one pass, to the byte.
 
 use std::{
+    cell::{Cell, RefCell},
     fmt,
     hash::{Hash, Hasher},
     ops::Range,
-    sync::Arc,
+    sync::{
+        Arc,
+        atomic::{AtomicU32, Ordering},
+    },
 };
 
 use base_db::{Db, Crate, EditionedFileId};
 use blaze_ids::Domain;
+use blaze_mem::scratch::{self, Scratch};
 use hir_expand::{HirFileId, span_map::SpanMap};
+use salsa::plumbing::{AsId, FromId};
 use span::{AstIdMap, ErasedFileAstId, RealSpanMap};
 use syntax::{
     AstNode, FlatTree, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxToken, TextRange, TextSize,
     ast::{self, HasModuleItem},
 };
+use triomphe::HeaderSlice;
 
 use crate::{
     attrs::parse_extra_crate_attrs,
     item_tree::{
         AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, Function, IdRange, ItemKind,
-        ItemTree, Macro2, MacroCall, MacroRules, Mod, ModItemId, NameId, PathId, RawVisibilityId,
-        Static, Struct, Trait, TypeAlias, Union, Use, UseTree, UseTreeId, lower,
+        ItemTree, ItemTreeLens, Macro2, MacroCall, MacroRules, Mod, ModItemId, NameId, PathId,
+        RawVisibilityId, Static, Struct, Trait, TypeAlias, Union, Use, UseTree, UseTreeId, block,
+        lower,
     },
 };
 
 #[cfg(test)]
 mod tests;
 
-/// The tree of a source file, from its items' ([`assemble`]).
+/// The tree of a source file, from its items' ([`assemble`]): the parts listed in the worker's
+/// kept list, the tree one allocation.
 pub(super) fn file_item_tree(
     db: &Db,
     file: EditionedFileId,
     krate: Crate,
 ) -> ItemTree {
     let top = top_attrs(db, file, krate);
-    // alloc: once a run, the list of the file's parts
-    let fragments: Vec<&ItemTree> =
-        item_sources(db, file, krate).iter().map(|&item| item_fragment(db, item)).collect();
-    assemble(top, &fragments)
+    let sources = item_sources(db, file, krate);
+    block::with_refs(|parts| {
+        for item in sources.iter() {
+            parts.push(item_fragment(db, item));
+        }
+        assemble(top, parts.as_slice())
+    })
 }
 
 /// One top-level item of a file, to lower on its own: its identity is its syntax's
@@ -67,10 +79,10 @@
 /// until every item of theirs is edited or gone; each is a file's text and words once.
 pub(super) struct ItemSyntax {
     key: ItemKey,
-    file: Arc<FileSyntax>,
-    /// The item's element in `file.tree`.
+    file: FileSyntax,
+    /// The item's element in the file's tree.
     at: u32,
-    /// The item's of `file.ids` and `file.anchors`.
+    /// The item's of the file's ids and anchors, in entries.
     ids: Range<u32>,
     anchors: Range<u32>,
 }
@@ -83,18 +95,64 @@
     fingerprint: u128,
 }
 
-/// The parse a file's [`ItemSyntax`]es were cut from, shared by all of them.
-struct FileSyntax {
+/// The parse a file's [`ItemSyntax`]es were cut from and the arrays they share, in one
+/// allocation: the words of the file's ids, then of its anchors ([`FileHead`]), then two a
+/// source, the file's [`ItemSource`]s ([`ItemSources`]). The words are atomic so the sources,
+/// made from items that hold this block, can be written into it after; every word is written
+/// before the block is the query's value, and none after.
+type FileSyntax = triomphe::Arc<HeaderSlice<FileHead, [AtomicU32]>>;
+
+struct FileHead {
     tree: Arc<FlatTree>,
-    /// Per item, as [`lower::Ctx::for_item`] reads them: `[start, kind, id]` of each item node
-    /// lowering asks the id of, from where the top-level item starts, by start.
-    ids: Box<[[u32; 3]]>,
-    /// Per item: `[start, id]` of each span anchor within it that is one of those nodes, from
-    /// where it starts, by start; the first the item's own. The anchors of an `impl`'s or a
-    /// `trait`'s items are left out: lowering looks up no span past their start.
-    anchors: Box<[[u32; 2]]>,
+    /// How many of the ids, per item as [`lower::Ctx::for_item`] reads them: `[start, kind, id]`
+    /// of each item node lowering asks the id of, from where the top-level item starts, by
+    /// start.
+    ids: u32,
+    /// How many of the anchors, per item: `[start, id]` of each span anchor within it that is
+    /// one of those nodes, from where it starts, by start; the first the item's own. The
+    /// anchors of an `impl`'s or a `trait`'s items are left out: lowering looks up no span past
+    /// their start.
+    anchors: u32,
 }
 
+/// The words of a [`FileSyntax`]'s sources, two a source, the id's low half first.
+fn slots(file: &FileSyntax) -> &[[AtomicU32; 2]] {
+    let from = 3 * file.header.ids as usize + 2 * file.header.anchors as usize;
+    file.slice[from..].as_chunks::<2>().0
+}
+
+/// The top-level items of a file, in order ([`item_sources`]): the last words of the block
+/// their syntax shares.
+pub(super) struct ItemSources {
+    file: FileSyntax,
+}
+
+impl ItemSources {
+    fn bits(&self) -> impl ExactSizeIterator<Item = u64> + '_ {
+        slots(&self.file).iter().map(|[low, high]| {
+            u64::from(low.load(Ordering::Relaxed)) | u64::from(high.load(Ordering::Relaxed)) << 32
+        })
+    }
+
+    pub(super) fn iter<'db>(&'db self) -> impl ExactSizeIterator<Item = ItemSource<'db>> + 'db {
+        self.bits().map(|bits| ItemSource::from_id(salsa::Id::from_bits(bits)))
+    }
+}
+
+impl PartialEq for ItemSources {
+    fn eq(&self, other: &Self) -> bool {
+        self.bits().eq(other.bits())
+    }
+}
+
+impl Eq for ItemSources {}
+
+impl fmt::Debug for ItemSources {
+    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
+        f.debug_list().entries(self.iter()).finish()
+    }
+}
+
 impl PartialEq for ItemSyntax {
     fn eq(&self, other: &Self) -> bool {
         self.key == other.key
@@ -118,34 +176,73 @@
     }
 }
 
-fn slice<T>(all: &[T], range: &Range<u32>) -> &[T] {
-    &all[range.start as usize..range.end as usize]
+fn count(len: usize) -> u32 {
+    u32::try_from(len).expect("a file's syntax indexes in `u32`")
 }
 
 fn range(range: Range<usize>) -> Range<u32> {
-    let end = u32::try_from(range.end).expect("a file's syntax indexes in `u32`");
-    range.start as u32..end
+    range.start as u32..count(range.end)
 }
 
 impl ItemSyntax {
+    fn id_words(&self) -> &[AtomicU32] {
+        &self.file.slice[3 * self.ids.start as usize..3 * self.ids.end as usize]
+    }
+
+    fn anchor_words(&self) -> &[AtomicU32] {
+        let from = 3 * self.file.header.ids as usize;
+        let (start, end) = (self.anchors.start as usize, self.anchors.end as usize);
+        &self.file.slice[from + 2 * start..from + 2 * end]
+    }
+
+    /// The item's tree; what it allocates is the tree's block and what its values need, its ids
+    /// and anchors read from the worker's kept words.
     fn lower(&self, db: &Db, krate: Crate) -> ItemTree {
-        let file = &*self.file;
-        let root = syntax::Tree::new(file.tree.clone()).root(); // clone: an `Arc`, the cursor's
+        let root = root_of(&self.file.header.tree);
         let NodeOrToken::Node(node) = root.flat_element(self.at) else {
             panic!("the element of the item {:?} is a token", self.key.ast_id)
         };
         let item = ast::Item::cast(node).expect("an item's element is an item");
         let item_range = item.syntax().text_range();
-        let pairs = slice(&file.anchors, &self.anchors)
-            .iter()
-            .map(|&[start, id]| (TextSize::new(start), ErasedFileAstId::from_raw(id)))
-            .collect(); // alloc: the item's span map, once a lowering of a changed item
-        let spans = RealSpanMap::from_file(self.key.file, pairs, item_range.len());
-        let ids = slice(&file.ids, &self.ids);
-        lower::Ctx::for_item(db, krate, ids, &spans, item_range.start()).lower_item(&item)
+        let load = |word: &AtomicU32| word.load(Ordering::Relaxed);
+        scratch::with_u32s(|words| {
+            words.extend(self.id_words().iter().map(load));
+            let (ids, anchors) = {
+                let split = words.len();
+                words.extend(self.anchor_words().iter().map(load));
+                words.split_at(split)
+            };
+            let (ids, anchors) = (ids.as_chunks::<3>().0, anchors.as_chunks::<2>().0);
+            lower::Ctx::for_item(db, krate, ids, anchors, self.key.file, item_range)
+                .lower_item(&item)
+        })
     }
 }
 
+thread_local! {
+    /// The root cursor of the tree this worker last lowered an item of. A file's items are
+    /// lowered one after another, so they share the one root, which a cursor allocates; it
+    /// keeps that tree alive until the worker lowers an item of another.
+    static ROOT: RefCell<Option<SyntaxNode>> = const { RefCell::new(None) };
+}
+
+/// A root cursor of `tree`, the worker's kept one when it is of `tree`.
+fn root_of(tree: &Arc<FlatTree>) -> SyntaxNode {
+    ROOT.with_borrow_mut(|root| {
+        if let Some(kept) = root.as_ref()
+            && kept.flat().is_some_and(|(it, _)| Arc::ptr_eq(it, tree))
+        {
+            // clone: a cursor, a count of the kept root
+            return kept.clone();
+        }
+        // clone: an `Arc`, the cursor's
+        let fresh = syntax::Tree::new(tree.clone()).root();
+        // clone: a cursor, a count of the root kept
+        *root = Some(fresh.clone());
+        fresh
+    })
+}
+
 /// The domain of an [`ItemKey`]'s fingerprint.
 static ITEM: Domain = Domain::new("blaze v2 item syntax 1");
 
@@ -170,32 +267,53 @@
 }
 
 /// The arrays [`item_sources`] fills, empty between runs and keeping their capacity, so a worker
-/// grows none again once it has cut a file as big. A scratch that grew past [`SCRATCH_IDS`]
-/// (a generated file) is dropped, so a worker keeps a bounded amount.
-#[derive(Default)]
-struct Scratch {
-    ids: Vec<[u32; 3]>,
-    anchors: Vec<[u32; 2]>,
-    items: Vec<(ItemKey, u32, Range<u32>, Range<u32>)>,
-    bytes: Vec<u8>,
+/// grows none again once it has cut a file as big. One that grew past [`SCRATCH_IDS`] (a
+/// generated file) shrinks back to it, so a worker keeps a bounded amount.
+struct Cutting {
+    ids: Scratch<[u32; 3]>,
+    anchors: Scratch<[u32; 2]>,
+    items: Scratch<Cut>,
 }
 
-thread_local! {
-    static SCRATCH: std::cell::Cell<Scratch> = std::cell::Cell::new(Scratch::default());
+impl Cutting {
+    const fn new() -> Cutting {
+        Cutting { ids: Scratch::new(), anchors: Scratch::new(), items: Scratch::new() }
+    }
+
+    fn shrink(&mut self) {
+        if self.ids.high_water() > SCRATCH_IDS {
+            self.ids.shrink(SCRATCH_IDS);
+        }
+        if self.anchors.high_water() > SCRATCH_IDS {
+            self.anchors.shrink(SCRATCH_IDS);
+        }
+        if self.items.high_water() > SCRATCH_IDS {
+            self.items.shrink(SCRATCH_IDS);
+        }
+    }
 }
 
-/// About 200 KiB of a [`Scratch`]'s arrays.
+thread_local! {
+    static SCRATCH: Cell<Cutting> = const { Cell::new(Cutting::new()) };
+}
+
+/// About 200 KiB of a [`Cutting`]'s arrays.
 const SCRATCH_IDS: usize = 1 << 14;
 
+/// One top-level item as [`cut`] finds it: its key, its element in its file's tree and its
+/// entries of the file's ids and anchors.
+struct Cut {
+    key: ItemKey,
+    at: u32,
+    ids: Range<u32>,
+    anchors: Range<u32>,
+}
+
 /// The top-level items of `file`, each cut out to lower on its own; one walk of the file's
-/// items, its ids and anchors in two arrays all the items share. A run allocates the two
-/// arrays, the `Arc` holding them and the list of items, and nothing for each item.
+/// items, its ids and anchors and the items' sources in one block they all share. A run
+/// allocates that block, and nothing for each item but what salsa keeps of its source.
 #[salsa::tracked(returns(ref))]
-pub(super) fn item_sources<'db>(
-    db: &'db Db,
-    file: EditionedFileId,
-    krate: Crate,
-) -> Box<[ItemSource<'db>]> {
+pub(super) fn item_sources<'db>(db: &'db Db, file: EditionedFileId, krate: Crate) -> ItemSources {
     let _p = tracing::info_span!("item_sources", ?file).entered();
     let source_file = file.parse(db).tree();
     let hir_file = HirFileId::from(file);
@@ -204,49 +322,79 @@
         unreachable!("a source file's span map is a real file's")
     };
     let file_id = file.span_file_id(db);
-    let (tree, _) = source_file.syntax().flat().expect("a source file is a flat tree's root");
-    let tree = tree.clone(); // clone: an `Arc`, the file's tree the items keep
 
-    let mut scratch = SCRATCH.take();
-    let Scratch { ids, anchors, items, bytes } = &mut scratch;
-    for item in source_file.items() {
-        let node = item.syntax();
-        let (_, at) = node.flat().expect("a top-level item is in its file's tree");
-        let item_range = node.text_range();
-        let ids_from = ids.len();
-        item_ids(&item, ast_id_map, item_range.start(), ids);
-        let anchors_from = anchors.len();
-        item_anchors(span_map, item_range, &ids[ids_from..], anchors);
-
-        bytes.clear();
-        let mut feed = Feed(&mut *bytes);
-        file_id.hash(&mut feed);
-        hash_item(node, &mut feed);
-        ids[ids_from..].hash(&mut feed);
-        anchors[anchors_from..].hash(&mut feed);
-        let key = ItemKey {
-            file: file_id,
-            ast_id: ErasedFileAstId::from_raw(ids[ids_from][2]),
-            fingerprint: feed.fingerprint(),
-        };
-        items.push((key, at, range(ids_from..ids.len()), range(anchors_from..anchors.len())));
+    let mut cutting = SCRATCH.replace(Cutting::new());
+    let Cutting { ids, anchors, items } = &mut cutting;
+    let (ids, anchors, items) = (ids.take(), anchors.take(), items.take());
+    let file = cut(&source_file, ast_id_map, span_map, file_id, ids, anchors, items);
+    for (slot, item) in slots(&file).iter().zip(items.drain(..)) {
+        let Cut { key, at, ids, anchors } = item;
+        // clone: an `Arc`, the file's block each item holds
+        let syntax = ItemSyntax { key, file: file.clone(), at, ids, anchors };
+        let bits = ItemSource::new(db, krate, syntax).as_id().as_bits();
+        slot[0].store(bits as u32, Ordering::Relaxed);
+        slot[1].store((bits >> 32) as u32, Ordering::Relaxed);
     }
+    cutting.shrink();
+    SCRATCH.set(cutting);
+    ItemSources { file }
+}
 
-    // alloc: once a run, the arrays every item of the file shares
-    let file = Arc::new(FileSyntax { tree, ids: ids[..].into(), anchors: anchors[..].into() });
-    let sources = (items.drain(..))
-        .map(|(key, at, ids, anchors)| {
-            // clone: an `Arc`, the file's syntax each item holds
-            let file = file.clone();
-            ItemSource::new(db, krate, ItemSyntax { key, file, at, ids, anchors })
-        })
-        .collect(); // alloc: the query's value, once a run
-    ids.clear();
-    anchors.clear();
-    if ids.capacity() <= SCRATCH_IDS {
-        SCRATCH.set(scratch);
-    }
-    sources
+/// Cuts `source_file` into its top-level items, one walk: `items` gets each one's key, element
+/// and entries, `ids` and `anchors` the file's, and the block they go into comes back, with a
+/// slot of two zero words for each item's source after. Allocates the block alone, each item's
+/// bytes hashed in one BLAKE3 pass from the worker's kept buffer.
+fn cut(
+    source_file: &ast::SourceFile,
+    ast_id_map: &AstIdMap,
+    span_map: &RealSpanMap,
+    file_id: span::EditionedFileId,
+    ids: &mut Vec<[u32; 3]>,
+    anchors: &mut Vec<[u32; 2]>,
+    items: &mut Vec<Cut>,
+) -> FileSyntax {
+    let (tree, _) = source_file.syntax().flat().expect("a source file is a flat tree's root");
+    scratch::with_bytes(|bytes| {
+        for item in source_file.items() {
+            let node = item.syntax();
+            let (_, at) = node.flat().expect("a top-level item is in its file's tree");
+            let item_range = node.text_range();
+            let ids_from = ids.len();
+            item_ids(&item, ast_id_map, item_range.start(), ids);
+            let anchors_from = anchors.len();
+            item_anchors(span_map, item_range, &ids[ids_from..], anchors);
+
+            bytes.clear();
+            let mut feed = Feed(&mut *bytes);
+            file_id.hash(&mut feed);
+            hash_item(node, &mut feed);
+            ids[ids_from..].hash(&mut feed);
+            anchors[anchors_from..].hash(&mut feed);
+            let key = ItemKey {
+                file: file_id,
+                ast_id: ErasedFileAstId::from_raw(ids[ids_from][2]),
+                fingerprint: feed.fingerprint(),
+            };
+            let (id_range, anchor_range) =
+                (range(ids_from..ids.len()), range(anchors_from..anchors.len()));
+            items.push(Cut { key, at, ids: id_range, anchors: anchor_range });
+        }
+    });
+
+    let (ids, anchors) = (ids.as_flattened(), anchors.as_flattened());
+    let words = ids.len() + anchors.len() + 2 * items.len();
+    let word = |at: usize| match at.checked_sub(ids.len()) {
+        None => ids[at],
+        Some(at) => anchors.get(at).copied().unwrap_or(0),
+    };
+    let head = FileHead {
+        // clone: an `Arc`, the file's tree the items keep
+        tree: tree.clone(),
+        ids: count(ids.len() / 3),
+        anchors: count(anchors.len() / 2),
+    };
+    // alloc: once a run, the block every item of the file shares
+    triomphe::Arc::from_header_and_iter(head, (0..words).map(|at| AtomicU32::new(word(at))))
 }
 
 /// The ids of `item` and the items it is made of, the nodes [`lower::Ctx`] asks the ids of, in
@@ -424,227 +572,172 @@
 
 /// A file's tree from its parts: `top`, its top level's attributes ([`top_attrs`]), and its
 /// items' trees in order, each one's ids moved by where its columns start. One pass over every
-/// part, each column allocated once at its length. The attributes are cloned, which allocates
-/// for those whose paths or inputs do.
+/// part into the tree's one block, sized from the parts' summed lengths: one allocation. The
+/// attributes and `cfg`s are cloned, which allocates for those whose inputs or nested `cfg`s do.
 pub(super) fn assemble(top: &ItemTree, fragments: &[&ItemTree]) -> ItemTree {
-    macro_rules! column {
-        ($field:ident) => {
-            Vec::with_capacity(
-                top.$field.len() + fragments.iter().map(|it| it.$field.len()).sum::<usize>(),
-            )
-        };
+    let mut lens = ItemTreeLens {
+        attr_owners: top.attr_owners().len(),
+        attr_list: top.attr_list().len(),
+        cfgs: top.cfgs().len(),
+        ..ItemTreeLens::default()
+    };
+    debug_assert_eq!(
+        ItemTreeLens::of(top),
+        lens,
+        "a top level's tree is its attributes alone ([`lower::Ctx::lower_top`])"
+    );
+    for part in fragments {
+        lens.add(&ItemTreeLens::of(part));
     }
-    let mut top_level = column!(top_level);
-    let mut kinds = column!(kinds);
-    let mut data = column!(data);
-    let mut ast_ids = column!(ast_ids);
-    let mut children = column!(children);
-    let mut consts = column!(consts);
-    let mut enums = column!(enums);
-    let mut extern_blocks = column!(extern_blocks);
-    let mut extern_crates = column!(extern_crates);
-    let mut functions = column!(functions);
-    let mut impls = column!(impls);
-    let mut macro2s = column!(macro2s);
-    let mut macro_calls = column!(macro_calls);
-    let mut macro_rules = column!(macro_rules);
-    let mut mods = column!(mods);
-    let mut statics = column!(statics);
-    let mut structs = column!(structs);
-    let mut traits = column!(traits);
-    let mut type_aliases = column!(type_aliases);
-    let mut unions = column!(unions);
-    let mut uses = column!(uses);
-    let mut use_trees = column!(use_trees);
-    let mut use_tree_lists = column!(use_tree_lists);
-    let mut ctor_vis = column!(ctor_vis);
-    let mut names = column!(names);
-    let mut paths = column!(paths);
-    let mut vis = column!(vis);
-    let mut attr_owners = column!(attr_owners);
-    let mut attrs = column!(attrs);
-    let mut cfgs = column!(cfgs);
-
-    attrs.extend(top.attrs.iter().cloned());
-    cfgs.extend(top.cfgs.iter().cloned());
-    for part in fragments {
-        let base = Bases {
-            item: base(kinds.len()),
-            children: base(children.len()),
-            use_trees: base(use_trees.len()),
-            use_tree_lists: base(use_tree_lists.len()),
-            ctor_vis: base(ctor_vis.len()),
-            names: base(names.len()),
-            paths: base(paths.len()),
-            vis: base(vis.len()),
-            attrs: base(attrs.len()),
-            cfgs: base(cfgs.len()),
-        };
-        for (&kind, &at) in part.kinds.iter().zip(&part.data) {
-            let column = match kind {
-                ItemKind::Const => consts.len(),
-                ItemKind::Enum => enums.len(),
-                ItemKind::ExternBlock => extern_blocks.len(),
-                ItemKind::ExternCrate => extern_crates.len(),
-                ItemKind::Function => functions.len(),
-                ItemKind::Impl => impls.len(),
-                ItemKind::Macro2 => macro2s.len(),
-                ItemKind::MacroCall => macro_calls.len(),
-                ItemKind::MacroRules => macro_rules.len(),
-                ItemKind::Mod => mods.len(),
-                ItemKind::Static => statics.len(),
-                ItemKind::Struct => structs.len(),
-                ItemKind::Trait => traits.len(),
-                ItemKind::TypeAlias => type_aliases.len(),
-                ItemKind::Union => unions.len(),
-                ItemKind::Use => uses.len(),
+    lens.attr_index = lens.kinds.div_ceil(64);
+    ItemTree::build(lens, |s| {
+        s.attr_list.extend(top.attr_list().iter().cloned());
+        s.cfgs.extend(top.cfgs().iter().cloned());
+        for part in fragments {
+            let base = Bases {
+                item: base(s.kinds.len()),
+                children: base(s.children.len()),
+                use_trees: base(s.use_trees.len()),
+                use_tree_lists: base(s.use_tree_lists.len()),
+                ctor_vis: base(s.ctor_vis.len()),
+                names: base(s.names.len()),
+                paths: base(s.paths.len()),
+                vis: base(s.vis.len()),
+                attrs: base(s.attr_list.len()),
+                cfgs: base(s.cfgs.len()),
             };
-            kinds.push(kind);
-            data.push(base(column) + at);
-        }
-        ast_ids.extend_from_slice(&part.ast_ids);
-        top_level.extend(part.top_level.iter().map(|&it| base.item(it)));
-        children.extend(part.children.iter().map(|&it| base.item(it)));
+            for (&kind, &at) in part.kinds().iter().zip(part.data()) {
+                let column = match kind {
+                    ItemKind::Const => s.consts.len(),
+                    ItemKind::Enum => s.enums.len(),
+                    ItemKind::ExternBlock => s.extern_blocks.len(),
+                    ItemKind::ExternCrate => s.extern_crates.len(),
+                    ItemKind::Function => s.functions.len(),
+                    ItemKind::Impl => s.impls.len(),
+                    ItemKind::Macro2 => s.macro2s.len(),
+                    ItemKind::MacroCall => s.macro_calls.len(),
+                    ItemKind::MacroRules => s.macro_rules.len(),
+                    ItemKind::Mod => s.mods.len(),
+                    ItemKind::Static => s.statics.len(),
+                    ItemKind::Struct => s.structs.len(),
+                    ItemKind::Trait => s.traits.len(),
+                    ItemKind::TypeAlias => s.type_aliases.len(),
+                    ItemKind::Union => s.unions.len(),
+                    ItemKind::Use => s.uses.len(),
+                };
+                s.kinds.push(kind);
+                s.data.push(base(column) + at);
+            }
+            s.ast_ids.extend_from_slice(part.ast_ids());
+            s.top_level.extend(part.top_level().iter().map(|&it| base.item(it)));
+            s.children.extend(part.children().iter().map(|&it| base.item(it)));
 
-        consts.extend(
-            part.consts
-                .iter()
-                .map(|it| Const { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        enums.extend(
-            part.enums
-                .iter()
-                .map(|it| Enum { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        extern_blocks.extend(
-            part.extern_blocks
-                .iter()
-                .map(|it| ExternBlock { children: base.children(it.children) }),
-        );
-        extern_crates.extend(part.extern_crates.iter().map(|it| ExternCrate {
-            name: base.name(it.name),
-            alias: base.alias(it.alias),
-            visibility: base.vis(it.visibility),
-        }));
-        functions.extend(
-            part.functions.iter().map(|it| Function {
+            s.consts.extend(
+                part.consts().iter().map(|it| Const {
+                    name: base.name(it.name),
+                    visibility: base.vis(it.visibility),
+                }),
+            );
+            s.enums.extend(
+                part.enums().iter().map(|it| Enum {
+                    name: base.name(it.name),
+                    visibility: base.vis(it.visibility),
+                }),
+            );
+            s.extern_blocks.extend(
+                part.extern_blocks()
+                    .iter()
+                    .map(|it| ExternBlock { children: base.children(it.children) }),
+            );
+            s.extern_crates.extend(part.extern_crates().iter().map(|it| ExternCrate {
                 name: base.name(it.name),
+                alias: base.alias(it.alias),
                 visibility: base.vis(it.visibility),
-            }),
-        );
-        impls.extend_from_slice(&part.impls);
-        macro2s.extend(
-            part.macro2s
-                .iter()
-                .map(|it| Macro2 { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        macro_calls.extend(
-            part.macro_calls.iter().map(|it| MacroCall { path: base.path(it.path), ..*it }),
-        );
-        macro_rules
-            .extend(part.macro_rules.iter().map(|it| MacroRules { name: base.name(it.name) }));
-        mods.extend(part.mods.iter().map(|it| Mod {
-            name: base.name(it.name),
-            visibility: base.vis(it.visibility),
-            items: match it.items {
-                IdRange::NONE => IdRange::NONE,
-                items => base.children(items),
-            },
-        }));
-        statics.extend(
-            part.statics
-                .iter()
-                .map(|it| Static { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        structs.extend(part.structs.iter().map(|it| Struct {
-            name: base.name(it.name),
-            visibility: base.vis(it.visibility),
-            value_ns_ctor: match it.value_ns_ctor.len {
-                0 => it.value_ns_ctor,
-                _ => shift(it.value_ns_ctor, base.ctor_vis),
-            },
-        }));
-        traits.extend(
-            part.traits
-                .iter()
-                .map(|it| Trait { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        type_aliases.extend(
-            part.type_aliases.iter().map(|it| TypeAlias {
+            }));
+            s.functions.extend(part.functions().iter().map(|it| Function {
                 name: base.name(it.name),
                 visibility: base.vis(it.visibility),
-            }),
-        );
-        unions.extend(
-            part.unions
-                .iter()
-                .map(|it| Union { name: base.name(it.name), visibility: base.vis(it.visibility) }),
-        );
-        uses.extend(part.uses.iter().map(|it| Use {
-            visibility: base.vis(it.visibility),
-            use_tree: base.use_tree(it.use_tree),
-        }));
-        use_trees.extend(part.use_trees.iter().map(|it| UseTree {
-            kind: it.kind,
-            path: base.path(it.path),
-            alias: base.alias(it.alias),
-            list: match it.kind {
-                UseTree::PREFIXED => shift(it.list, base.use_tree_lists),
-                _ => it.list,
-            },
-        }));
-        use_tree_lists.extend(part.use_tree_lists.iter().map(|&it| base.use_tree(it)));
-        ctor_vis.extend(part.ctor_vis.iter().map(|&it| base.vis(it)));
-        names.extend(part.names.iter().cloned());
-        paths.extend(part.paths.iter().cloned());
-        vis.extend(part.vis.iter().cloned());
-        attr_owners.extend(part.attr_owners.iter().map(|it| AttrOwner {
-            owner: base.item(it.owner),
-            attrs: shift(it.attrs, base.attrs),
-            cfg: match it.cfg {
-                AttrOwner::ENABLED => AttrOwner::ENABLED,
-                cfg => cfg + base.cfgs,
-            },
-        }));
-        attrs.extend(part.attrs.iter().cloned());
-        cfgs.extend(part.cfgs.iter().cloned());
-    }
-    attr_owners.extend_from_slice(&top.attr_owners);
-    let attr_index = super::attr_index(&attr_owners, kinds.len());
-
-    ItemTree {
-        top_level: top_level.into(),
-        kinds: kinds.into(),
-        data: data.into(),
-        ast_ids: ast_ids.into(),
-        children: children.into(),
-        consts: consts.into(),
-        enums: enums.into(),
-        extern_blocks: extern_blocks.into(),
-        extern_crates: extern_crates.into(),
-        functions: functions.into(),
-        impls: impls.into(),
-        macro2s: macro2s.into(),
-        macro_calls: macro_calls.into(),
-        macro_rules: macro_rules.into(),
-        mods: mods.into(),
-        statics: statics.into(),
-        structs: structs.into(),
-        traits: traits.into(),
-        type_aliases: type_aliases.into(),
-        unions: unions.into(),
-        uses: uses.into(),
-        use_trees: use_trees.into(),
-        use_tree_lists: use_tree_lists.into(),
-        ctor_vis: ctor_vis.into(),
-        names: names.into(),
-        paths: paths.into(),
-        vis: vis.into(),
-        attr_owners: attr_owners.into(),
-        attr_index,
-        attrs: attrs.into(),
-        cfgs: cfgs.into(),
-    }
+            }));
+            s.impls.extend_from_slice(part.impls());
+            s.macro2s.extend(part.macro2s().iter().map(|it| Macro2 {
+                name: base.name(it.name),
+                visibility: base.vis(it.visibility),
+            }));
+            s.macro_calls.extend(
+                part.macro_calls().iter().map(|it| MacroCall { path: base.path(it.path), ..*it }),
+            );
+            s.macro_rules.extend(
+                part.macro_rules().iter().map(|it| MacroRules { name: base.name(it.name) }),
+            );
+            s.mods.extend(part.mods().iter().map(|it| Mod {
+                name: base.name(it.name),
+                visibility: base.vis(it.visibility),
+                items: match it.items {
+                    IdRange::NONE => IdRange::NONE,
+                    items => base.children(items),
+                },
+            }));
+            s.statics.extend(part.statics().iter().map(|it| Static {
+                name: base.name(it.name),
+                visibility: base.vis(it.visibility),
+            }));
+            s.structs.extend(part.structs().iter().map(|it| Struct {
+                name: base.name(it.name),
+                visibility: base.vis(it.visibility),
+                value_ns_ctor: match it.value_ns_ctor.len {
+                    0 => it.value_ns_ctor,
+                    _ => shift(it.value_ns_ctor, base.ctor_vis),
+                },
+            }));
+            s.traits.extend(
+                part.traits().iter().map(|it| Trait {
+                    name: base.name(it.name),
+                    visibility: base.vis(it.visibility),
+                }),
+            );
+            s.type_aliases.extend(part.type_aliases().iter().map(|it| TypeAlias {
+                name: base.name(it.name),
+                visibility: base.vis(it.visibility),
+            }));
+            s.unions.extend(
+                part.unions().iter().map(|it| Union {
+                    name: base.name(it.name),
+                    visibility: base.vis(it.visibility),
+                }),
+            );
+            s.uses.extend(part.uses().iter().map(|it| Use {
+                visibility: base.vis(it.visibility),
+                use_tree: base.use_tree(it.use_tree),
+            }));
+            s.use_trees.extend(part.use_trees().iter().map(|it| UseTree {
+                kind: it.kind,
+                path: base.path(it.path),
+                alias: base.alias(it.alias),
+                list: match it.kind {
+                    UseTree::PREFIXED => shift(it.list, base.use_tree_lists),
+                    _ => it.list,
+                },
+            }));
+            s.use_tree_lists.extend(part.use_tree_lists().iter().map(|&it| base.use_tree(it)));
+            s.ctor_vis.extend(part.ctor_vis().iter().map(|&it| base.vis(it)));
+            s.names.extend_from_slice(part.names());
+            s.paths.extend_from_slice(part.paths());
+            s.vis.extend(part.vis().iter().cloned());
+            s.attr_owners.extend(part.attr_owners().iter().map(|it| AttrOwner {
+                owner: base.item(it.owner),
+                attrs: shift(it.attrs, base.attrs),
+                cfg: match it.cfg {
+                    AttrOwner::ENABLED => AttrOwner::ENABLED,
+                    cfg => cfg + base.cfgs,
+                },
+            }));
+            s.attr_list.extend(part.attr_list().iter().cloned());
+            s.cfgs.extend(part.cfgs().iter().cloned());
+        }
+        s.attr_owners.extend_from_slice(top.attr_owners());
+        let items = s.kinds.len();
+        s.attr_index.extend(super::attr_blocks(s.attr_owners.written(), items));
+    })
 }
 
 /// Where one part's columns start in the tree assembled.
```

### `crates/hir-def/src/item_tree/items/tests.rs`

As of 20:45: `assembly_allocates_once_a_column` tightened to `assembly_allocates_once_a_tree`
(`(1, 1)`), plus `a_tree_assembled_from_memo_hits_allocates_its_block`,
`a_warm_lowering_of_an_item_allocates_its_block`, `a_warm_cut_allocates_its_block` and
`spans_from_an_item_s_anchors_are_its_map_s`. With F, keep the assembly budget at once a tree
(the `Arc` aside) and `trees_asked_again_allocate_nothing`; the per-item ones go with
`item_sources`.

```diff
--- a/crates/hir-def/src/item_tree/items/tests.rs
+++ b/crates/hir-def/src/item_tree/items/tests.rs
@@ -1,14 +1,18 @@
 use std::{
     ops::Range,
     path::{Path, PathBuf},
+    sync::atomic::Ordering::Relaxed,
 };
 
 use base_db::EditionedFileId;
 use blaze_ids::rng::SplitMix64;
+use hir_expand::{HirFileId, span_map::SpanMap};
+use span::{ErasedFileAstId, RealSpanMap};
+use syntax::{NodeOrToken, TextRange, TextSize};
 use test_fixture::WithFixture;
 
 use crate::{
-    item_tree::{ItemTree, lower_whole, tests::assert_assembled_is_whole},
+    item_tree::{ItemTree, lower, lower_whole, tests::assert_assembled_is_whole},
     test_db::{TestDB, TestDBExt},
 };
 
@@ -198,15 +202,16 @@
     });
     blaze_alloc_count::assert_at_most(0, "memo hits of a file's parts", || {
         std::hint::black_box(super::top_attrs(&db, file, krate));
-        for &item in super::item_sources(&db, file, krate) {
+        for item in super::item_sources(&db, file, krate).iter() {
             std::hint::black_box(super::item_fragment(&db, item));
         }
     });
 }
 
-/// Assembling a file's tree allocates once a column, however many items the file has.
+/// Assembling a file's tree allocates its one block, however many items the file has: its
+/// names and paths are copied and its visibilities cloned, none of which allocates.
 #[test]
-fn assembly_allocates_once_a_column() {
+fn assembly_allocates_once_a_tree() {
     let allocations = |items: usize| {
         let text: String = (0..items)
             .map(|i| format!("pub struct S{i}(pub u8);\nfn f{i}() {{}}\nuse a::b{i};\n"))
@@ -215,11 +220,146 @@
         let krate = db.test_crate();
         let top = super::top_attrs(&db, file, krate);
         let fragments: Vec<&ItemTree> = (super::item_sources(&db, file, krate).iter())
-            .map(|&item| super::item_fragment(&db, item))
+            .map(|item| super::item_fragment(&db, item))
             .collect();
         let (tree, used) = blaze_alloc_count::measure(|| super::assemble(top, &fragments));
         assert_eq!(tree, lower_whole(&db, file.into(), krate));
         used.allocations
     };
-    assert_eq!(allocations(4), allocations(64));
+    assert_eq!((allocations(4), allocations(64)), (1, 1));
+}
+
+/// A file's tree assembled from its parts, the parts' memo hits: its block, the list of parts
+/// the worker's kept one.
+#[test]
+fn a_tree_assembled_from_memo_hits_allocates_its_block() {
+    let (db, file) = db_of(LOWERED);
+    let krate = db.test_crate();
+    let top = super::top_attrs(&db, file, krate);
+    let tree = super::file_item_tree(&db, file, krate);
+    let (again, used) = blaze_alloc_count::measure(|| super::file_item_tree(&db, file, krate));
+    assert_eq!(again, tree);
+    // The attribute `#[custom = "literal"]` is cloned with its boxed input.
+    assert_eq!(top.attr_list().len(), 0);
+    assert_eq!(used.allocations, 2);
+}
+
+/// One of every item kind, its values interned or `Copy` but for the one attribute with an
+/// input, a boxed literal.
+const LOWERED: &str = r#"
+pub struct S(pub u8);
+struct R { a: u8, b: u16 }
+union U { a: u8 }
+enum E { A, B(u8) }
+pub(crate) fn f(x: u8) -> u8 { x }
+use a::{b, c as d, e::*};
+pub(in crate::m) use self::m::inner;
+extern crate core as c;
+mod m { pub fn inner() {} struct T; }
+mod outline;
+const C: u8 = 1;
+static X: u8 = 0;
+trait Tr { fn t(&self); }
+impl Tr for S { fn t(&self) {} }
+type A = u8;
+macro_rules! mac { () => {}; }
+pub macro m2() {}
+mac!();
+extern "C" { fn e(); static E: u8; }
+#[cfg(false)] fn disabled() {}
+#[inline] fn flagged() {}
+#[custom = "literal"] fn attributed() {}
+"#;
+
+/// Lowering an item again on a warm worker allocates its tree's block and what its values
+/// need: nothing but for an attribute's input.
+#[test]
+fn a_warm_lowering_of_an_item_allocates_its_block() {
+    let (db, file) = db_of(LOWERED);
+    let krate = db.test_crate();
+    let mut seen = 0;
+    for item in super::item_sources(&db, file, krate).iter() {
+        let syntax = item.syntax(&db);
+        let first = syntax.lower(&db, krate);
+        let (tree, used) = blaze_alloc_count::measure(|| syntax.lower(&db, krate));
+        assert_eq!(tree, first, "{syntax:?}");
+        let inputs = tree.attr_list().iter().filter(|attr| attr.input.is_some()).count();
+        assert_eq!(used.allocations, 1 + inputs, "{syntax:?}: {tree:?}");
+        seen += 1;
+    }
+    assert_eq!(seen, 22, "every item of the file");
 }
+
+/// Cutting a file into its items on a warm worker allocates the block they share alone: the
+/// arrays are the worker's, kept, and each item's bytes hashed from its kept buffer.
+#[test]
+fn a_warm_cut_allocates_its_block() {
+    let (db, file) = db_of(LOWERED);
+    let source_file = file.parse(&db).tree();
+    let hir_file = HirFileId::from(file);
+    let ast_id_map = hir_file.ast_id_map(&db);
+    let SpanMap::RealSpanMap(span_map) = hir_file.span_map(&db) else {
+        unreachable!("a source file's span map is a real file's")
+    };
+    let file_id = file.span_file_id(&db);
+    let (mut ids, mut anchors, mut items) = (Vec::new(), Vec::new(), Vec::new());
+    let mut cut = || {
+        ids.clear();
+        anchors.clear();
+        items.clear();
+        let block = super::cut(
+            &source_file,
+            ast_id_map,
+            span_map,
+            file_id,
+            &mut ids,
+            &mut anchors,
+            &mut items,
+        );
+        (block, items.len())
+    };
+    let (first, count) = cut();
+    let ((block, again), used) = blaze_alloc_count::measure(&mut cut);
+    assert_eq!((again, super::slots(&block).len()), (count, count));
+    assert!(first.slice.iter().zip(&block.slice).all(|(a, b)| a.load(Relaxed) == b.load(Relaxed)));
+    assert_eq!(used.allocations, 1);
+}
+
+/// An item's spans read from its anchors in place are the spans of the map of the item alone.
+#[test]
+fn spans_from_an_item_s_anchors_are_its_map_s() {
+    let (db, file) = db_of(LOWERED);
+    let krate = db.test_crate();
+    for item in super::item_sources(&db, file, krate).iter() {
+        let syntax = item.syntax(&db);
+        let words: Vec<u32> = syntax.anchor_words().iter().map(|it| it.load(Relaxed)).collect();
+        let anchors = words.as_chunks::<2>().0;
+        let NodeOrToken::Node(node) =
+            super::root_of(&syntax.file.header.tree).flat_element(syntax.at)
+        else {
+            panic!("the element of {syntax:?} is a token")
+        };
+        let range = node.text_range();
+        let pairs = (anchors.iter())
+            .map(|&[start, id]| (TextSize::new(start), ErasedFileAstId::from_raw(id)))
+            .collect();
+        let map = RealSpanMap::from_file(syntax.key.file, pairs, range.len());
+        let spans = lower::Spans::Item { file: syntax.key.file, anchors, item: range };
+        let ends = [range.start(), range.start() + TextSize::new(1), range.end()];
+        for &start in &ends {
+            for &end in ends.iter().filter(|&&end| end >= start) {
+                let within = TextRange::new(start, end);
+                assert_eq!(
+                    spans.span_for_range(within),
+                    map.span_for_range(within - range.start()),
+                    "{syntax:?} at {within:?}"
+                );
+            }
+        }
+        for anchor in anchors {
+            let at = range.start() + TextSize::new(anchor[0]);
+            let within = TextRange::empty(at);
+            assert_eq!(spans.span_for_range(within), map.span_for_range(within - range.start()));
+        }
+    }
+}
```

### Budget tests that prove it

- `tests.rs`: `a_warm_builder_lowers_a_file_in_one_allocation` (1),
  `decoding_a_tree_allocates_its_block_and_its_paths` (1 + paths), and the kept
  `trees_asked_again_allocate_nothing` (0, memo hits).
- `items/tests.rs`: `assembly_allocates_once_a_tree` (1), once ported to F's `assemble`.
- `block.rs`'s own tests: columns read back what was written, an empty tree allocates nothing,
  a build that errors or panics drops what it wrote, and a short column panics.

### `Debug`

The derived `Debug` of `ItemTree` named the column `attrs`; it now prints it as `attr_list`.
No `expect!` in the tree prints `ItemTree`'s `Debug` (the pretty printer is separate).

Status: open
