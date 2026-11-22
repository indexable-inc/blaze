//! A struct-of-arrays vector, as Zig's `MultiArrayList`: one packed column per field, all in one
//! allocation, so a loop over one field reads only that field's bytes.
//!
//! [`Soa<C, A>`] is the vector over a tuple `C` of up to [`MOST_FIELDS`] field types; the
//! [`soa!`] macro names it: a row struct, the vector, a row of references and one slice
//! accessor per field, every one of them safe code over [`Soa`]. All the unsafe code is here,
//! once, for every row type.
//!
//! ```ignore
//! soa! {
//!     pub struct Span, Spans, SpanRef {
//!         pub start: u32 => starts / starts_mut,
//!         pub len: u16 => lens / lens_mut,
//!         pub file: FileId => files / files_mut,
//!     }
//! }
//! let mut spans = Spans::new_in(&frame);
//! spans.push(Span { start: 3, len: 4, file });
//! let total: u32 = spans.starts().iter().sum();
//! ```
//!
//! # Layout
//!
//! For capacity `n` the block is `n * row_size` bytes, aligned to the most aligned field:
//! the columns one after another, ordered by alignment from most to least (ties in declaration
//! order), so each column starts aligned with no padding anywhere (every size is a multiple of
//! its alignment, and alignments are powers of two). A column's start is its prefix (the sizes
//! of the fields before it in that order) times `n`; the starts are kept, recomputed only when
//! the block moves. Fields sized 0 take no bytes; a row of only such fields never allocates.
//!
//! Growth doubles: a new block, each column copied to its new start, the old block freed, so
//! growing moves every column.
//!
//! # Relation to `blaze_mem::column` and `AppendVec`
//!
//! Different concepts, not duplicates: blaze-mem's `Column` is one field, appended to
//! concurrently in a reserved region that never moves, read lock-free while written, and
//! `AppendVec` one writer with lock-free readers; this vector has one owner, moves when it
//! grows, holds many fields, and lives in any allocator. A blaze-mem column of rows that wants
//! several fields is several columns indexed alike.
//!
//! Not here yet: `extend`, `drain`.

pub use core::alloc::Allocator;
use core::{
    alloc::Layout,
    any::type_name,
    fmt,
    iter::FusedIterator,
    marker::PhantomData,
    mem::{DropGuard, align_of, size_of},
    num::NonZero,
    ptr::{self, NonNull},
    slice,
};

#[cfg(test)]
use crate::plant::{self, Plant};
use crate::{
    error::{AllocFailure, fail},
    vec::{Refused, Vec, out_of_bounds},
};

/// The most fields a row may have.
pub const MOST_FIELDS: usize = 12;

/// A field's size and alignment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    pub size: usize,
    pub align: usize,
}

impl Field {
    pub const fn of<T>() -> Field {
        Field { size: size_of::<T>(), align: align_of::<T>() }
    }
}

/// Each field's prefix: the sizes of the fields laid out before it (more aligned, or as aligned
/// and declared earlier).
const fn prefix_of(fields: &[Field]) -> [usize; MOST_FIELDS] {
    assert!(fields.len() <= MOST_FIELDS, "a struct-of-arrays row has at most MOST_FIELDS fields");
    let mut prefix = [0; MOST_FIELDS];
    let mut i = 0;
    while i < fields.len() {
        let mut sum = 0;
        let mut j = 0;
        while j < fields.len() {
            let more = fields[j].align > fields[i].align;
            let same_earlier = fields[j].align == fields[i].align && j < i;
            if more || same_earlier {
                sum += fields[j].size;
            }
            j += 1;
        }
        prefix[i] = sum;
        i += 1;
    }
    prefix
}

const fn row_size(fields: &[Field]) -> usize {
    let mut sum = 0;
    let mut i = 0;
    while i < fields.len() {
        sum += fields[i].size;
        i += 1;
    }
    sum
}

const fn row_align(fields: &[Field]) -> usize {
    let mut most = 1;
    let mut i = 0;
    while i < fields.len() {
        if fields[i].align > most {
            most = fields[i].align;
        }
        i += 1;
    }
    most
}

const fn most_size(fields: &[Field]) -> usize {
    let mut most = 0;
    let mut i = 0;
    while i < fields.len() {
        if fields[i].size > most {
            most = fields[i].size;
        }
        i += 1;
    }
    most
}

/// The index of field `name` in `names`, for [`soa!`]'s accessors; a name not there fails the
/// build.
#[doc(hidden)]
pub const fn index_of(names: &[&str], name: &str) -> usize {
    let mut i = 0;
    while i < names.len() {
        let (a, b) = (names[i].as_bytes(), name.as_bytes());
        if a.len() == b.len() {
            let mut k = 0;
            while k < a.len() && a[k] == b[k] {
                k += 1;
            }
            if k == a.len() {
                return i;
            }
        }
        i += 1;
    }
    panic!("soa!: no field of that name");
}

/// Where each column starts, in declaration order.
#[doc(hidden)]
#[derive(Clone, Copy)]
pub struct Cols {
    starts: [*mut u8; MOST_FIELDS],
}

impl Cols {
    /// Row `row` of column `field`, as a `T`.
    ///
    /// # Safety
    ///
    /// Column `field` holds `T`s and `row` is below the capacity.
    #[inline]
    unsafe fn at<T>(self, field: usize, row: usize) -> *mut T {
        // SAFETY: the column has room for the capacity's rows (the caller's promise).
        unsafe { self.starts[field].cast::<T>().add(row) }
    }
}

mod sealed {
    pub trait Sealed {}
}

/// A tuple of field types a [`Soa`] can hold: implemented here for tuples of 1 to
/// [`MOST_FIELDS`] types, and nowhere else (it is sealed), so the layout constants and the
/// unsafe methods are this crate's alone.
pub trait Columns: Sized + sealed::Sealed {
    #[doc(hidden)]
    const FIELDS: &'static [Field];
    #[doc(hidden)]
    const PREFIX: [usize; MOST_FIELDS] = prefix_of(Self::FIELDS);
    #[doc(hidden)]
    const ROW_SIZE: usize = row_size(Self::FIELDS);
    #[doc(hidden)]
    const ALIGN: usize = row_align(Self::FIELDS);
    #[doc(hidden)]
    const MOST_SIZE: usize = most_size(Self::FIELDS);

    /// A row by reference: a tuple of references, one per column.
    type Ref<'a>
    where
        Self: 'a;
    /// A row by mutable reference.
    type Mut<'a>
    where
        Self: 'a;

    /// # Safety
    ///
    /// Row `row` of every column is in the block and empty.
    #[doc(hidden)]
    unsafe fn write(self, cols: Cols, row: usize);

    /// # Safety
    ///
    /// Row `row` of every column holds a value, which the caller then treats as moved out.
    #[doc(hidden)]
    unsafe fn read(cols: Cols, row: usize) -> Self;

    /// # Safety
    ///
    /// Row `row` holds values, not written for `'a`.
    #[doc(hidden)]
    unsafe fn get<'a>(cols: Cols, row: usize) -> Self::Ref<'a>
    where
        Self: 'a;

    /// # Safety
    ///
    /// Row `row` holds values, not otherwise read or written for `'a`.
    #[doc(hidden)]
    unsafe fn get_mut<'a>(cols: Cols, row: usize) -> Self::Mut<'a>
    where
        Self: 'a;

    /// Drops rows `from..to` of every column; every column is dropped, a panic in one among
    /// the ways.
    ///
    /// # Safety
    ///
    /// Those rows hold values, which the caller then treats as gone.
    #[doc(hidden)]
    unsafe fn drop_rows(cols: Cols, from: usize, to: usize);

    /// Reorders the first `order.len()` rows of every column so row `k` is the row that was at
    /// `order[k]`, through `scratch`.
    ///
    /// # Safety
    ///
    /// `order` is a permutation of `0..order.len()`, those rows hold values, and `scratch` has
    /// `order.len() * MOST_SIZE` bytes aligned to `ALIGN`, unused by anything else.
    #[doc(hidden)]
    unsafe fn permute(cols: Cols, order: &[usize], scratch: *mut u8);
}

/// Column `I`'s type.
pub trait ColumnAt<const I: usize>: Columns {
    type T;
}

/// Rows whose every field is `Clone`: a row clones from its references.
pub trait CloneColumns: Columns {
    #[doc(hidden)]
    fn clone_row(row: Self::Ref<'_>) -> Self;

    /// Whether every field's clone is a bitwise copy (`TrivialClone`, `spec`), so the rows clone
    /// by one block copy per column. Known at compile time for each tuple.
    #[doc(hidden)]
    fn trivial() -> bool;
}

/// Clones of `src`'s rows into `out`, which is empty with room for them: one block copy per
/// column when every field is `TrivialClone`, else a row at a time (a panicking clone leaves
/// `out` holding the rows made).
fn clone_rows<C: CloneColumns, A: Allocator, B: Allocator>(src: &Soa<C, A>, out: &mut Soa<C, B>) {
    if C::trivial() {
        for (field, info) in C::FIELDS.iter().enumerate() {
            // SAFETY: column `field` of `src` holds `len` values; `out`'s has room for them and
            // is empty, in another block. Each field's clone is its bitwise copy
            // (`TrivialClone`, which `trivial` says of every field).
            unsafe { ptr::copy_nonoverlapping(src.cols.starts[field], out.cols.starts[field], info.size * src.len) };
        }
        out.len = src.len;
    } else {
        for row in src.iter() {
            // The room is reserved: the push does not allocate.
            out.push(C::clone_row(row));
        }
    }
}

/// Rows whose every field is `Debug`.
pub trait DebugColumns: Columns {
    #[doc(hidden)]
    fn debug_row(row: Self::Ref<'_>, list: &mut fmt::DebugList<'_, '_>);
}

/// Moves the rows of one column into the order `order` names, through `scratch`.
///
/// # Safety
///
/// As [`Columns::permute`], for this one column.
unsafe fn permute_one<T>(column: *mut T, order: &[usize], scratch: *mut T) {
    for (k, &from) in order.iter().enumerate() {
        // SAFETY: `from` is a row holding a value, read once (a permutation); slot `k` of the
        // scratch is in it and empty. Bitwise moves: nothing is dropped or duplicated.
        unsafe { scratch.add(k).write(column.add(from).read()) };
    }
    // SAFETY: the scratch now holds every row, in order; the column's rows are moved-from, so
    // they are overwritten without a drop.
    unsafe { ptr::copy_nonoverlapping(scratch, column, order.len()) };
}

/// Moves `rows` rows of every column from row `from` to row `to`, bitwise; the ranges may
/// overlap.
///
/// # Safety
///
/// Both ranges are below the capacity of the block `cols` is in; the source holds values, which
/// the caller then counts at the destination.
unsafe fn shift<C: Columns>(cols: Cols, from: usize, to: usize, rows: usize) {
    for (field, info) in C::FIELDS.iter().enumerate() {
        let start = cols.starts[field];
        // SAFETY: both ranges are in the column (the caller's promise); `copy` allows the
        // overlap. A field sized 0 copies no bytes.
        unsafe { ptr::copy(start.add(from * info.size), start.add(to * info.size), rows * info.size) };
    }
}

/// The state of a [`Soa::retain`]: rows below `write` are kept, `write..read` are empty,
/// `read..total` not yet seen.
struct Compact<'a, C: Columns> {
    len: &'a mut usize,
    cols: Cols,
    read: usize,
    write: usize,
    total: usize,
    _rows: PhantomData<C>,
}

impl<C: Columns> Compact<'_, C> {
    /// Moves the rows not yet seen down to the kept ones and sets the length: the end of a
    /// compaction, or its unwinding.
    fn finish(c: Compact<'_, C>) {
        let unseen = c.total - c.read;
        if c.read != c.write {
            // SAFETY: rows `read..total` hold values; `write..` below them are empty.
            unsafe { shift::<C>(c.cols, c.read, c.write, unseen) };
        }
        *c.len = c.write + unseen;
    }
}

// The `@` arms come first, so a recursive call never tries the main arm.
macro_rules! tuple_columns {
    (@each $all:tt $($i:tt $t:ident),+) => {
        $( tuple_columns!(@one $all $i $t); )+
    };
    (@one [$($all:ident),+] $i:tt $t:ident) => {
        impl<$($all),+> ColumnAt<$i> for ($($all,)+) {
            type T = $t;
        }
    };
    ($($i:tt $t:ident),+) => {
        impl<$($t: Clone),+> CloneColumns for ($($t,)+) {
            fn clone_row(row: Self::Ref<'_>) -> Self {
                ($( row.$i.clone(), )+)
            }

            #[inline]
            fn trivial() -> bool {
                true $( && <$t as crate::spec::CloneSlice>::trivial() )+
            }
        }

        impl<$($t: fmt::Debug),+> DebugColumns for ($($t,)+) {
            fn debug_row(row: Self::Ref<'_>, list: &mut fmt::DebugList<'_, '_>) {
                list.entry(&row);
            }
        }

        impl<$($t),+> sealed::Sealed for ($($t,)+) {}

        impl<$($t),+> Columns for ($($t,)+) {
            const FIELDS: &'static [Field] = &[$(Field::of::<$t>()),+];

            type Ref<'a> = ($(&'a $t,)+) where Self: 'a;
            type Mut<'a> = ($(&'a mut $t,)+) where Self: 'a;

            #[inline]
            unsafe fn write(self, cols: Cols, row: usize) {
                // SAFETY: row `row` of each column is in the block and empty (the caller's
                // promise), column `$i` holding `$t`s.
                $( unsafe { cols.at::<$t>($i, row).write(self.$i) }; )+
            }

            #[inline]
            unsafe fn read(cols: Cols, row: usize) -> Self {
                // SAFETY: row `row` of each column holds a value (the caller's promise).
                ($( unsafe { cols.at::<$t>($i, row).read() }, )+)
            }

            #[inline]
            unsafe fn get<'a>(cols: Cols, row: usize) -> Self::Ref<'a>
            where
                Self: 'a,
            {
                // SAFETY: row `row` holds values not written for `'a` (the caller's promise).
                ($( unsafe { &*cols.at::<$t>($i, row) }, )+)
            }

            #[inline]
            unsafe fn get_mut<'a>(cols: Cols, row: usize) -> Self::Mut<'a>
            where
                Self: 'a,
            {
                // SAFETY: as `get`, with no other access for `'a`; the columns are disjoint.
                ($( unsafe { &mut *cols.at::<$t>($i, row) }, )+)
            }

            unsafe fn drop_rows(cols: Cols, from: usize, to: usize) {
                // One guard per column, all dropped at the end of the scope: a panicking drop in
                // one column still drops the others.
                $(
                    let _guard = DropGuard::new((), move |()| {
                        // SAFETY: rows `from..to` of column `$i` hold `$t`s (the caller's
                        // promise), each dropped once.
                        let run = unsafe { cols.at::<$t>($i, from) };
                        // SAFETY: as above.
                        unsafe { ptr::drop_in_place(ptr::slice_from_raw_parts_mut(run, to - from)) };
                    });
                )+
            }

            unsafe fn permute(cols: Cols, order: &[usize], scratch: *mut u8) {
                // SAFETY: as the trait's contract, one column at a time; the scratch is aligned
                // to the most aligned field and holds `order.len()` of the largest.
                $( unsafe { permute_one::<$t>(cols.at::<$t>($i, 0), order, scratch.cast::<$t>()) }; )+
            }
        }

        tuple_columns!(@each [$($t),+] $($i $t),+);
    };
}

tuple_columns!(0 T0);
tuple_columns!(0 T0, 1 T1);
tuple_columns!(0 T0, 1 T1, 2 T2);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9, 10 T10);
tuple_columns!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7, 8 T8, 9 T9, 10 T10, 11 T11);

/// A struct-of-arrays vector of rows `C` (a tuple of field types) in `A`: one block, one packed column a field, so a pass over one field reads only that field's bytes.
///
/// # Invariants
///
/// `base` is a block of `alloc` for `Soa::layout(cap)` when `cap != 0` and rows are not sized
/// 0, else dangling aligned to `C::ALIGN`; `cols` are the columns' starts in it for `cap`; rows
/// `0..len` of every column hold values; `len <= cap`; `cap` is `usize::MAX` when rows are
/// sized 0.
pub struct Soa<C: Columns, A: Allocator> {
    base: NonNull<u8>,
    cols: Cols,
    len: usize,
    cap: usize,
    alloc: A,
    /// Owns `C`s, for drop check and for `Send`/`Sync`.
    _owns: PhantomData<C>,
}

// SAFETY: as `Vec<C, A>`'s: the vector owns its rows and its block, nothing shared.
unsafe impl<C: Columns + Send, A: Allocator + Send> Send for Soa<C, A> {}
// SAFETY: as `Vec<C, A>`'s: `&Soa` gives only `&` to rows and to the allocator.
unsafe impl<C: Columns + Sync, A: Allocator + Sync> Sync for Soa<C, A> {}

impl<C: Columns, A: Allocator> Soa<C, A> {
    fn dangling() -> NonNull<u8> {
        // `ALIGN` is a power of two, so never 0.
        let Some(align) = NonZero::new(C::ALIGN) else { unreachable!("an alignment is not 0") };
        NonNull::without_provenance(align)
    }

    fn layout(cap: usize) -> Option<Layout> {
        let size = C::ROW_SIZE.checked_mul(cap)?;
        Layout::from_size_align(size, C::ALIGN).ok()
    }

    fn starts(base: NonNull<u8>, cap: usize) -> Cols {
        let mut starts = [ptr::null_mut(); MOST_FIELDS];
        for (field, start) in starts.iter_mut().enumerate().take(C::FIELDS.len()) {
            // A row of fields sized 0 has every prefix 0, so its capacity of `usize::MAX`
            // multiplies to 0; otherwise `prefix * cap <= row_size * cap`, the block's size.
            let offset = C::PREFIX[field].wrapping_mul(cap);
            *start = base.as_ptr().wrapping_add(offset);
        }
        Cols { starts }
    }

    pub fn new_in(alloc: A) -> Soa<C, A> {
        let base = Soa::<C, A>::dangling();
        let cap = if C::ROW_SIZE == 0 { usize::MAX } else { 0 };
        Soa { base, cols: Soa::<C, A>::starts(base, cap), len: 0, cap, alloc, _owns: PhantomData }
    }

    pub fn with_capacity_in(capacity: usize, alloc: A) -> Soa<C, A> {
        let mut soa = Soa::new_in(alloc);
        soa.reserve(capacity);
        soa
    }

    pub fn try_with_capacity_in(capacity: usize, alloc: A) -> Result<Soa<C, A>, AllocFailure> {
        let mut soa = Soa::new_in(alloc);
        soa.try_reserve(capacity)?;
        Ok(soa)
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    #[inline]
    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn allocator(&self) -> &A {
        &self.alloc
    }

    pub fn reserve(&mut self, additional: usize) {
        if let Err(failure) = self.try_reserve(additional) {
            fail(failure);
        }
    }

    /// Room for `additional` more rows, doubling; the vector is as it was on failure.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocFailure> {
        let what = type_name::<C>();
        let overflow = AllocFailure::CapacityOverflow { what, len: self.len, additional };
        let needed = self.len.checked_add(additional).ok_or(overflow)?;
        if needed <= self.cap {
            return Ok(());
        }
        // `cap <= isize::MAX` (a block's size bounds it), so doubling cannot overflow.
        let new_cap = needed.max(self.cap * 2).max(4);
        self.grow_to(new_cap)
    }

    #[cold]
    fn grow_to(&mut self, new_cap: usize) -> Result<(), AllocFailure> {
        let what = type_name::<C>();
        let additional = new_cap - self.len;
        let overflow = AllocFailure::CapacityOverflow { what, len: self.len, additional };
        let layout = Soa::<C, A>::layout(new_cap).ok_or(overflow)?;
        let block = self.alloc.allocate(layout).map_err(|_| AllocFailure::Refused { what, layout })?;
        let base = block.cast::<u8>();
        let cols = Soa::<C, A>::starts(base, new_cap);
        for (field, info) in C::FIELDS.iter().enumerate() {
            #[cfg(test)]
            if plant::on(Plant::SoaGrowSkipsColumn) && field + 1 == C::FIELDS.len() {
                continue;
            }
            // SAFETY: the old column holds `len` values of `info.size` bytes; the new one has
            // room for `new_cap >= len`; distinct blocks. The bytes move: the old block is freed
            // below without a drop.
            unsafe { ptr::copy_nonoverlapping(self.cols.starts[field], cols.starts[field], info.size * self.len) };
        }
        if self.cap != 0 {
            let Some(old) = Soa::<C, A>::layout(self.cap) else { unreachable!("the block was made with it") };
            // SAFETY: the old block is `alloc`'s for that layout (the invariant), freed once.
            unsafe { self.alloc.deallocate(self.base, old) };
        }
        self.base = base;
        self.cols = cols;
        self.cap = new_cap;
        Ok(())
    }

    pub fn push(&mut self, row: C) {
        if self.len == self.cap {
            self.reserve(1);
        }
        // SAFETY: `len < cap`: row `len` of every column is in the block and empty.
        unsafe { row.write(self.cols, self.len) };
        self.len += 1;
    }

    /// [`Soa::push`], or the row back with the [`AllocFailure`].
    pub fn try_push(&mut self, row: C) -> Result<(), Refused<C>> {
        if self.len == self.cap
            && let Err(why) = self.try_reserve(1)
        {
            return Err(Refused { value: row, why });
        }
        // SAFETY: as `push`.
        unsafe { row.write(self.cols, self.len) };
        self.len += 1;
        Ok(())
    }

    pub fn pop(&mut self) -> Option<C> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: the last row held values the length no longer counts.
        Some(unsafe { C::read(self.cols, self.len) })
    }

    pub fn get(&self, row: usize) -> Option<C::Ref<'_>> {
        if row >= self.len {
            return None;
        }
        // SAFETY: `row < len` holds values; `&self` keeps them unwritten.
        Some(unsafe { C::get(self.cols, row) })
    }

    pub fn get_mut(&mut self, row: usize) -> Option<C::Mut<'_>> {
        if row >= self.len {
            return None;
        }
        // SAFETY: as `get`, `&mut self` the only access.
        Some(unsafe { C::get_mut(self.cols, row) })
    }

    /// Puts `new` at `row` and gives the row that was there.
    #[track_caller]
    pub fn replace(&mut self, row: usize, new: C) -> C {
        if row >= self.len {
            out_of_bounds("Soa::replace", row, self.len);
        }
        // SAFETY: `row < len` holds values: read out, then the emptied slots written, nothing
        // that can panic between.
        unsafe {
            let old = C::read(self.cols, row);
            new.write(self.cols, row);
            old
        }
    }

    /// Puts `new` at `row`, dropping the row that was there.
    #[track_caller]
    pub fn set(&mut self, row: usize, new: C) {
        drop(self.replace(row, new));
    }

    /// Takes row `row` out and puts the last row in its place: constant time, the order changed.
    #[track_caller]
    pub fn remove_swap(&mut self, row: usize) -> C {
        if row >= self.len {
            out_of_bounds("Soa::remove_swap", row, self.len);
        }
        let last = self.len - 1;
        // SAFETY: `row` and `last` hold values. `row` is read out; when they differ, the last
        // row moves into the emptied slots; the length then stops counting `last`.
        unsafe {
            let taken = C::read(self.cols, row);
            if row != last {
                C::read(self.cols, last).write(self.cols, row);
            }
            self.len = last;
            taken
        }
    }

    /// Drops the rows past `len`; nothing when it is no longer.
    pub fn truncate(&mut self, len: usize) {
        if len >= self.len {
            return;
        }
        let old = self.len;
        // The length goes first: a panicking drop leaves only rows not cut.
        self.len = len;
        // SAFETY: rows `len..old` held values the length no longer counts.
        unsafe { C::drop_rows(self.cols, len, old) };
    }

    pub fn clear(&mut self) {
        self.truncate(0);
    }

    /// Puts `new` at `row`, moving the rows after it back one: `O(len - row)` per column.
    #[track_caller]
    pub fn insert_shift(&mut self, row: usize, new: C) {
        if row > self.len {
            out_of_bounds("Soa::insert_shift", row, self.len);
        }
        if self.len == self.cap {
            self.reserve(1);
        }
        // SAFETY: `len < cap`: rows `row..len` move to `row + 1..len + 1`, in the capacity; row
        // `row` is then empty and written.
        unsafe {
            shift::<C>(self.cols, row, row + 1, self.len - row);
            new.write(self.cols, row);
        }
        self.len += 1;
    }

    /// Takes row `row` out, moving the rows after it forward one: `O(len - row)` per column.
    #[track_caller]
    pub fn remove_shift(&mut self, row: usize) -> C {
        if row >= self.len {
            out_of_bounds("Soa::remove_shift", row, self.len);
        }
        // SAFETY: row `row` holds values, read out once; rows `row + 1..len` then move into its
        // place and the length stops counting the last.
        unsafe {
            let taken = C::read(self.cols, row);
            shift::<C>(self.cols, row + 1, row, self.len - row - 1);
            self.len -= 1;
            taken
        }
    }

    /// Keeps the rows `keep(self, row)` says yes to, in order.
    ///
    /// `keep` is asked of every row first, with the vector whole (a panic in it changes
    /// nothing); then the rows said no to are dropped and the rest moved down, a panicking drop
    /// keeping the rows not yet seen. Memory: one flag per row, from the vector's allocator.
    pub fn retain(&mut self, mut keep: impl FnMut(&Soa<C, A>, usize) -> bool) {
        let len = self.len;
        // The flags borrow only the allocator, which the compaction below does not touch.
        let mut kept = Vec::with_capacity_in(len, &self.alloc);
        for row in 0..len {
            kept.push(keep(self, row));
        }
        if kept.iter().all(|&k| k) {
            return;
        }
        let cols = self.cols;
        // Nothing counts as in the vector while it is compacted; the guard sets the length.
        self.len = 0;
        let compact = Compact::<C> { len: &mut self.len, cols, read: 0, write: 0, total: len, _rows: PhantomData };
        let mut at = DropGuard::new(compact, Compact::finish);
        while at.read < len {
            let read = at.read;
            // Counted as gone before its drop, so a panic there does not drop it again.
            at.read += 1;
            if !kept[read] {
                // SAFETY: row `read` holds values no longer counted, read out once; dropping the
                // tuple drops every field, a panic in one among the ways.
                drop(unsafe { C::read(cols, read) });
                continue;
            }
            let write = at.write;
            if read != write {
                // SAFETY: row `read` holds values; row `write < read` was emptied.
                unsafe { shift::<C>(cols, read, write, 1) };
            }
            at.write += 1;
        }
    }

    /// Column `I`, one slice of every row's field `I`.
    pub fn column<const I: usize>(&self) -> &[<C as ColumnAt<I>>::T]
    where
        C: ColumnAt<I>,
    {
        // SAFETY: column `I` holds `len` values of its type from its start, aligned (the
        // layout); `&self` keeps them unwritten.
        unsafe { slice::from_raw_parts(self.cols.starts[I].cast::<<C as ColumnAt<I>>::T>(), self.len) }
    }

    pub fn column_mut<const I: usize>(&mut self) -> &mut [<C as ColumnAt<I>>::T]
    where
        C: ColumnAt<I>,
    {
        // SAFETY: as `column`, `&mut self` the only access.
        unsafe { slice::from_raw_parts_mut(self.cols.starts[I].cast::<<C as ColumnAt<I>>::T>(), self.len) }
    }

    /// The rows by reference, in order.
    pub fn iter(&self) -> Iter<'_, C> {
        Iter { cols: self.cols, at: 0, end: self.len, _rows: PhantomData }
    }

    /// Sorts the rows by `key(self, row)`, stable, moving every column once by one permutation.
    ///
    /// Cost: `len` calls of `key`, all made before any row moves (a panic in one leaves the
    /// rows as they were); `O(len log len)` comparisons of keys (a panicking comparison too
    /// leaves the rows as they were); then per column two moves of each row, through one
    /// scratch block of `len` times the largest field. Memory: the keys with their rows, `len`
    /// indices and the scratch, all from the vector's allocator and freed before it returns.
    pub fn sort_by_key<K: Ord>(&mut self, mut key: impl FnMut(&Soa<C, A>, usize) -> K) {
        let len = self.len;
        // Rows of fields sized 0 cannot be told apart: their order is not a thing to change,
        // and `key` is not called.
        if len < 2 || C::ROW_SIZE == 0 {
            return;
        }
        let this: &Soa<C, A> = self;
        let mut keyed = Vec::with_capacity_in(len, &this.alloc);
        for row in 0..len {
            keyed.push((key(this, row), row));
        }
        // By key, then by row: stable, and an unstable sort needs no buffer.
        keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut order = Vec::with_capacity_in(len, &this.alloc);
        order.extend(keyed.iter().map(|&(_, row)| row));
        drop(keyed);
        let bytes = C::MOST_SIZE * len;
        let Ok(layout) = Layout::from_size_align(bytes, C::ALIGN) else {
            unreachable!("no larger than the block, whose layout was made")
        };
        let Ok(scratch) = this.alloc.allocate(layout) else {
            fail(AllocFailure::Refused { what: type_name::<C>(), layout });
        };
        let scratch = scratch.cast::<u8>();
        let alloc = &this.alloc;
        let _free = DropGuard::new(scratch, move |scratch| {
            // SAFETY: the scratch is `alloc`'s for `layout`, freed once.
            unsafe { alloc.deallocate(scratch, layout) };
        });
        // SAFETY: `order` is a permutation of `0..len` (each row once, sorted), rows `0..len`
        // hold values, and the scratch is sized and aligned as `permute` asks. The writes go
        // through the block's pointers, no reference to a row being alive.
        unsafe { C::permute(this.cols, &order, scratch.as_ptr()) };
    }
}

// SAFETY (`may_dangle`): the drop drops the `C`s it owns (`PhantomData<C>` says so) and reads
// nothing else of them.
unsafe impl<#[may_dangle] C: Columns, A: Allocator> Drop for Soa<C, A> {
    fn drop(&mut self) {
        let (base, cap, len, cols) = (self.base, self.cap, self.len, self.cols);
        // The block is freed after the rows, a panicking drop among the ways.
        let _free = DropGuard::new(&self.alloc, move |alloc: &A| {
            if C::ROW_SIZE != 0 && cap != 0 {
                let Some(layout) = Soa::<C, A>::layout(cap) else { unreachable!("the block was made with it") };
                // SAFETY: the block is `alloc`'s for that layout (the invariant), freed once.
                unsafe { alloc.deallocate(base, layout) };
            }
        });
        // SAFETY: rows `0..len` hold values, dropped once; nothing reads them after.
        unsafe { C::drop_rows(cols, 0, len) };
    }
}

/// The old names, whose cost the name did not say; each forwards to the one that says it.
impl<C: Columns, A: Allocator> Soa<C, A> {
    #[deprecated(note = "renamed `remove_swap`, beside `remove_shift`")]
    #[track_caller]
    #[inline]
    pub fn swap_remove(&mut self, row: usize) -> C {
        self.remove_swap(row)
    }

    #[deprecated(note = "linear: use `insert_shift`, which says so")]
    #[track_caller]
    #[inline]
    pub fn insert(&mut self, row: usize, new: C) {
        self.insert_shift(row, new);
    }

    #[deprecated(note = "linear: use `remove_shift`, or `remove_swap` when order does not matter")]
    #[track_caller]
    #[inline]
    pub fn remove(&mut self, row: usize) -> C {
        self.remove_shift(row)
    }
}

impl<C: CloneColumns, A: Allocator> Soa<C, A> {
    /// A copy in another allocator: every column in one block of exactly `len` rows in `alloc`,
    /// each row cloned in turn (a row's fields are cloned by `Clone`, so a field that owns memory
    /// copies into its own allocator; the columns are what moves to `alloc`). A refusal or a
    /// panicking clone drops the rows made and frees the block.
    pub fn try_clone_in<B: Allocator>(&self, alloc: B) -> Result<Soa<C, B>, AllocFailure> {
        let mut out = Soa::try_with_capacity_in(self.len, alloc)?;
        clone_rows(self, &mut out);
        Ok(out)
    }

    /// [`Soa::try_clone_in`], panicking with the [`AllocFailure`].
    #[track_caller]
    pub fn clone_in<B: Allocator>(&self, alloc: B) -> Soa<C, B> {
        match self.try_clone_in(alloc) {
            Ok(copy) => copy,
            Err(failure) => fail(failure),
        }
    }
}

impl<C: CloneColumns, A: Allocator + Clone> Clone for Soa<C, A> {
    /// Allocates from the collection's own allocator: prefer `clone_in`. Each row cloned in
    /// turn into a block of exactly `len` rows; a panicking clone drops the clones made and
    /// frees the block.
    fn clone(&self) -> Soa<C, A> {
        let mut out = Soa::with_capacity_in(self.len, self.alloc.clone());
        clone_rows(self, &mut out);
        out
    }
}

impl<C: DebugColumns, A: Allocator> fmt::Debug for Soa<C, A> {
    /// As a list of rows, each a tuple of its fields.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut list = f.debug_list();
        for row in self.iter() {
            C::debug_row(row, &mut list);
        }
        list.finish()
    }
}

/// A [`Soa`]'s rows by reference.
pub struct Iter<'a, C: Columns> {
    cols: Cols,
    at: usize,
    end: usize,
    _rows: PhantomData<&'a C>,
}

// SAFETY: as `slice::Iter<'a, C>`'s: it gives `&` to rows.
unsafe impl<C: Columns + Sync> Send for Iter<'_, C> {}
// SAFETY: as `Send`.
unsafe impl<C: Columns + Sync> Sync for Iter<'_, C> {}

impl<'a, C: Columns + 'a> Iterator for Iter<'a, C> {
    type Item = C::Ref<'a>;

    fn next(&mut self) -> Option<C::Ref<'a>> {
        if self.at == self.end {
            return None;
        }
        let row = self.at;
        self.at += 1;
        // SAFETY: `row < len` of the vector borrowed for `'a`, which keeps it unwritten.
        Some(unsafe { C::get(self.cols, row) })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.end - self.at;
        (left, Some(left))
    }
}

impl<'a, C: Columns + 'a> DoubleEndedIterator for Iter<'a, C> {
    fn next_back(&mut self) -> Option<C::Ref<'a>> {
        if self.at == self.end {
            return None;
        }
        self.end -= 1;
        // SAFETY: as `next`.
        Some(unsafe { C::get(self.cols, self.end) })
    }
}

impl<'a, C: Columns + 'a> ExactSizeIterator for Iter<'a, C> {}

// SAFETY: `size_hint` is `end - at`, exactly the rows left.
unsafe impl<C: Columns> core::iter::TrustedLen for Iter<'_, C> {}
impl<'a, C: Columns + 'a> FusedIterator for Iter<'a, C> {}

/// Names a [`Soa`]: a row struct, its vector, a row of references, and a slice accessor per
/// field (`field: Type => cols / cols_mut`; `macro_rules!` cannot make a plural, so the names
/// are given). Up to [`MOST_FIELDS`] fields. Everything it writes is safe code over [`Soa`].
#[macro_export]
macro_rules! soa {
    (
        $(#[$meta:meta])*
        $vis:vis struct $Row:ident, $Vec:ident, $Ref:ident {
            $(
                $(#[$fmeta:meta])*
                $fvis:vis $field:ident : $ty:ty => $cols:ident / $cols_mut:ident
            ),+ $(,)?
        }
    ) => {
        // The row's attributes (docs, derives) and each field's go on the row struct only: the
        // reference row and the vector are the macro's, and an attribute such as a derive's
        // helper need not mean anything on them.
        $(#[$meta])*
        $vis struct $Row {
            $( $(#[$fmeta])* $fvis $field: $ty, )+
        }

        #[doc = concat!("One row of a `", stringify!($Vec), "`, by reference.")]
        #[derive(Clone, Copy)]
        #[allow(dead_code)]
        $vis struct $Ref<'a> {
            $( $fvis $field: &'a $ty, )+
        }

        #[doc = concat!("`", stringify!($Row), "`s, one column per field.")]
        $vis struct $Vec<A: $crate::soa::Allocator> {
            raw: $crate::soa::Soa<($($ty,)+), A>,
        }

        #[allow(dead_code)]
        impl $Row {
            #[doc(hidden)]
            const __SOA_FIELDS: &'static [&'static str] = &[$(stringify!($field)),+];

            #[doc(hidden)]
            #[inline]
            fn __from_columns(($($field,)+): ($($ty,)+)) -> $Row {
                $Row { $($field),+ }
            }

            #[doc(hidden)]
            #[inline]
            fn __into_columns(self) -> ($($ty,)+) {
                let $Row { $($field),+ } = self;
                ($($field,)+)
            }
        }

        impl<'a> $Ref<'a> {
            #[doc(hidden)]
            #[inline]
            fn __from_columns(($($field,)+): ($(&'a $ty,)+)) -> $Ref<'a> {
                $Ref { $($field),+ }
            }
        }

        #[allow(dead_code)]
        impl<A: $crate::soa::Allocator> $Vec<A> {
            pub fn new_in(alloc: A) -> $Vec<A> {
                $Vec { raw: $crate::soa::Soa::new_in(alloc) }
            }

            pub fn with_capacity_in(capacity: usize, alloc: A) -> $Vec<A> {
                $Vec { raw: $crate::soa::Soa::with_capacity_in(capacity, alloc) }
            }

            pub fn len(&self) -> usize {
                self.raw.len()
            }

            pub fn is_empty(&self) -> bool {
                self.raw.is_empty()
            }

            pub fn capacity(&self) -> usize {
                self.raw.capacity()
            }

            pub fn allocator(&self) -> &A {
                self.raw.allocator()
            }

            pub fn reserve(&mut self, additional: usize) {
                self.raw.reserve(additional);
            }

            pub fn try_reserve(&mut self, additional: usize) -> Result<(), $crate::AllocFailure> {
                self.raw.try_reserve(additional)
            }

            pub fn push(&mut self, row: $Row) {
                self.raw.push(row.__into_columns());
            }

            pub fn try_push(&mut self, row: $Row) -> Result<(), $crate::Refused<$Row>> {
                self.raw.try_push(row.__into_columns()).map_err(|refused| $crate::Refused {
                    value: $Row::__from_columns(refused.value),
                    why: refused.why,
                })
            }

            pub fn pop(&mut self) -> Option<$Row> {
                self.raw.pop().map($Row::__from_columns)
            }

            pub fn get(&self, row: usize) -> Option<$Ref<'_>> {
                self.raw.get(row).map($Ref::__from_columns)
            }

            #[track_caller]
            pub fn replace(&mut self, row: usize, new: $Row) -> $Row {
                $Row::__from_columns(self.raw.replace(row, new.__into_columns()))
            }

            #[track_caller]
            pub fn set(&mut self, row: usize, new: $Row) {
                self.raw.set(row, new.__into_columns());
            }

            /// Costs and panics as `Soa::remove_swap`'s.
            #[track_caller]
            pub fn remove_swap(&mut self, row: usize) -> $Row {
                $Row::__from_columns(self.raw.remove_swap(row))
            }

            /// Costs and panics as `Soa::insert_shift`'s: linear.
            #[track_caller]
            pub fn insert_shift(&mut self, row: usize, new: $Row) {
                self.raw.insert_shift(row, new.__into_columns());
            }

            /// Costs and panics as `Soa::remove_shift`'s: linear.
            #[track_caller]
            pub fn remove_shift(&mut self, row: usize) -> $Row {
                $Row::__from_columns(self.raw.remove_shift(row))
            }

            #[deprecated(note = "renamed `remove_swap`, beside `remove_shift`")]
            #[track_caller]
            pub fn swap_remove(&mut self, row: usize) -> $Row {
                self.remove_swap(row)
            }

            #[deprecated(note = "linear: use `insert_shift`, which says so")]
            #[track_caller]
            pub fn insert(&mut self, row: usize, new: $Row) {
                self.insert_shift(row, new);
            }

            #[deprecated(note = "linear: use `remove_shift`, or `remove_swap` when order does not matter")]
            #[track_caller]
            pub fn remove(&mut self, row: usize) -> $Row {
                self.remove_shift(row)
            }

            /// Keeps the rows `keep` says yes to; costs and panics as `Soa::retain`'s.
            pub fn retain(&mut self, mut keep: impl FnMut($Ref<'_>) -> bool) {
                self.raw.retain(|rows, row| match rows.get(row) {
                    Some(columns) => keep($Ref::__from_columns(columns)),
                    None => unreachable!("retain asks only for rows below the length"),
                });
            }

            pub fn truncate(&mut self, len: usize) {
                self.raw.truncate(len);
            }

            pub fn clear(&mut self) {
                self.raw.clear();
            }

            pub fn iter(
                &self,
            ) -> impl DoubleEndedIterator<Item = $Ref<'_>> + ExactSizeIterator {
                self.raw.iter().map($Ref::__from_columns)
            }

            /// Sorts the rows by `key`, stable; costs as `Soa::sort_by_key`'s.
            pub fn sort_by_key<K: Ord>(&mut self, mut key: impl FnMut($Ref<'_>) -> K) {
                self.raw.sort_by_key(|rows, row| match rows.get(row) {
                    Some(columns) => key($Ref::__from_columns(columns)),
                    None => unreachable!("the sort asks only for rows below the length"),
                });
            }

            /// The vector underneath, for what the names do not cover.
            pub fn as_soa(&self) -> &$crate::soa::Soa<($($ty,)+), A> {
                &self.raw
            }

            $(
                #[doc = concat!("Every row's `", stringify!($field), "`, as one slice.")]
                pub fn $cols(&self) -> &[$ty] {
                    self.raw.column::<{ $crate::soa::index_of($Row::__SOA_FIELDS, stringify!($field)) }>()
                }

                pub fn $cols_mut(&mut self) -> &mut [$ty] {
                    self.raw.column_mut::<{ $crate::soa::index_of($Row::__SOA_FIELDS, stringify!($field)) }>()
                }
            )+
        }

        // Bounded through `A`, so a row type whose fields are not all `Clone` (or `Debug`) just
        // goes without the impl.
        impl<A: $crate::soa::Allocator> Clone for $Vec<A>
        where
            $crate::soa::Soa<($($ty,)+), A>: Clone,
        {
            /// Allocates from the collection's own allocator: prefer `clone_in`.
            fn clone(&self) -> $Vec<A> {
                $Vec { raw: self.raw.clone() }
            }
        }

        // Bounded through `B` (a bound on the row tuple alone would be a trivial bound, an error
        // for a row with a field that is not `Clone`), so such a row goes without these.
        #[allow(dead_code)]
        impl<A: $crate::soa::Allocator> $Vec<A> {
            /// A copy in another allocator, as [`Soa::try_clone_in`]($crate::soa::Soa::try_clone_in).
            pub fn try_clone_in<B: $crate::soa::Allocator>(&self, alloc: B) -> Result<$Vec<B>, $crate::AllocFailure>
            where
                $crate::soa::Soa<($($ty,)+), A>: $crate::make::CloneIn<B, In = $crate::soa::Soa<($($ty,)+), B>>,
            {
                Ok($Vec { raw: $crate::make::CloneIn::try_clone_in(&self.raw, alloc)? })
            }

            /// A copy in another allocator, as [`Soa::clone_in`]($crate::soa::Soa::clone_in).
            #[track_caller]
            pub fn clone_in<B: $crate::soa::Allocator>(&self, alloc: B) -> $Vec<B>
            where
                $crate::soa::Soa<($($ty,)+), A>: $crate::make::CloneIn<B, In = $crate::soa::Soa<($($ty,)+), B>>,
            {
                $Vec { raw: $crate::make::CloneIn::clone_in(&self.raw, alloc) }
            }
        }

        impl<A: $crate::soa::Allocator> core::fmt::Debug for $Vec<A>
        where
            $crate::soa::Soa<($($ty,)+), A>: core::fmt::Debug,
        {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Debug::fmt(&self.raw, f)
            }
        }
    };
}
