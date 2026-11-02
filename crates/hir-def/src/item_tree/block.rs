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
//! The safe version, a `Box<[T]>` a column, costs an allocation and a free for each non-empty
//! column of every tree (a dozen or more for an item tree) and two words a column in the tree
//! itself, where this is one allocation and one pointer. [`columns!`]'s expansion is the one
//! place outside this module that holds `unsafe`, each item marked `#[allow(unsafe_code)]`.

use std::{
    alloc::{self, Layout},
    marker::PhantomData,
    mem,
    ptr::{self, NonNull},
    slice,
};

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
        // in it (what a building lends of itself is only what its sinks wrote, and this sink is
        // borrowed mutably), so the two don't overlap; the copies of `Copy` values own nothing.
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
        #[allow(unsafe_code)]
        unsafe impl Send for $Tree {}
        // SAFETY: as for `Send`.
        #[allow(unsafe_code)]
        unsafe impl Sync for $Tree {}
        const _: () = {
            fn send_sync<T: Send + Sync>() {}
            let _: fn() = || { $( send_sync::<$T>(); )+ };
        };

        impl $Tree {
            $(
                $(#[$col_attr])*
                #[inline]
                #[allow(unsafe_code)]
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

        #[allow(unsafe_code)]
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
}
