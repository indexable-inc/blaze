//! blaze v2's data format (docs/v2/plan/design/data.md section 5): **the bytes in memory are the
//! bytes on disk**. This crate is the part that defines and reads records, on `core` alone;
//! blaze-format adds the tiers, the cursor and the codecs, and re-exports all of this at the
//! paths it always had.
//!
//! A long-lived structure (a query's result, a pack of the store, the daemon's image) is a
//! tree of [`Record`]s in one byte buffer: no pointer, no `Arc`, aligned, little-endian, and
//! position-independent, each reference a [`Rel`] or a [`Slice`] counted from where it sits.
//! So a buffer is mmapped and read in place, and is the same bytes wherever it is mapped.
//!
//! A buffer starts with a [`Header`] naming its [`SchemaHash`]: a reader of another schema
//! refuses it by name ([`FormatError::Schema`]) and the daemon rebuilds what it held from the
//! store's results. A [`Persist`] type is one a query can return: written once into a buffer
//! in an allocator its caller names, read in place as its `Persist::Ref`.
//!
//! A reference always points at bytes written before the record that holds it: a [`Writer`]
//! writes children first and the root last. [`Record::check`] refuses any other reference, so
//! checking a buffer always ends, and costs what the records reached from the root are. Keep
//! columns of plain values ([`Slice`]s of integers): following one checks only its bounds.
//!
//! Spans are never in a record that is hashed: they sit in a side table (blaze-format's
//! `Spans`), and a record names one by [`SpanRef`].
//!
//! # `core` only
//!
//! With no feature on, this crate is `#![no_std]` with no `extern crate alloc`, nothing here
//! names `Global`, and the expansion of [`record!`] names `::core` and this crate only, with no
//! `unsafe` (the [`declared`] module says why that is sound): a `#![no_std]` crate that forbids
//! `unsafe_code` declares, writes ([`Writer`]`<`[`Own`]`<A>>`, made by [`Writer::new`] with the
//! allocator named, into `blaze_own::Vec<u8, A>`), persists ([`Persist`], [`to_bytes`]) and
//! reads records. The `alloc` feature adds `alloc`'s `Vec<u8, A>` as a writer's buffer, still in
//! a named allocator, for the crates not yet moved off it, and the two deprecated items on the
//! global heap that callers not yet ported use (`Writer::on_the_global_heap_until_ported`,
//! `Aligned`).
//!
//! The core is sans-IO: it reaches no clock, environment or IO. `#![blaze::sans_io]` is on each
//! module, not the root, for the reason the root's comment gives; `declared.rs` and
//! `verified.rs`, which define the exported macros, carry it nowhere for the same reason.
//!
//! # Nightly features
//!
//! | feature | what for |
//! |---|---|
//! | `type_info` | reading a declared record's struct by reflection ([`declared`]) |
//! | `const_cmp`, `const_trait_impl` | comparing two `TypeId`s in a constant |
#![no_std]
#![feature(type_info, const_cmp, const_trait_impl)]
// `register_tool` for `#[blaze::args]` (writer.rs) and each module's `#![blaze::sans_io]`.
#![feature(register_tool, custom_inner_attributes)]
#![register_tool(blaze)]
// No `#![blaze::trust(unsafe, ..)]` and no `#![blaze::sans_io]` here, though the crate holds
// `unsafe` (`raw.rs`, the impls of `Record` here and in `declared.rs`) and is a sans-IO core:
// rustc treats a crate whose root carries a tool inner attribute as macro-expanded, and then
// refuses `$crate::__record_fields!`, the recursion `record!` builds its field list with
// (`macro_expanded_macro_exports_accessed_by_absolute_paths`, rust-lang/rust#52234). The lint
// that asks for the first, `blaze::workspace_unsafe`, is allow; the second is on each module that
// defines no exported macro (as blaze-sim does).
#![deny(blaze::sans_io_core)]

// Only `alloc`'s `Vec<u8, A>` as a writer's buffer and the two deprecated global-heap items.
#[cfg(feature = "alloc")]
extern crate alloc;
// Tests use std's collections and printing; the crate itself never sees `std`.
#[cfg(test)]
extern crate std;

pub mod declared;
mod offset;
mod placed;
mod planted;
mod raw;
pub mod rows;
mod schema;
mod verified;
mod writer;
#[cfg(test)]
mod verified_tests;
#[cfg(test)]
mod writer_tests;

use core::{
    alloc::Allocator,
    fmt,
    marker::PhantomData,
    mem::{align_of, offset_of, size_of},
};

use blaze_ids::{ContentId, Fingerprint};

pub use crate::offset::{Offset, Unfit};
pub use crate::placed::Placed;
pub use crate::verified::{Items, Text, Verified};
pub use crate::schema::{Part, SCHEMA_DOMAIN, schema, schema_of};
pub use crate::writer::{
    AlignedIn, Own, Persist, WRITER_CAPACITY, WriteBuffer, Writer, WriterIn, WriterOpts,
    read_verified, to_bytes,
};
// Deprecated, on the global heap: for the callers not yet ported (writer.rs says which).
#[cfg(feature = "alloc")]
pub use crate::writer::Aligned;

#[cfg(target_endian = "big")]
compile_error!("blaze's format is little-endian, as every target blaze runs on is");

/// The hash of a record type's layout: its fields' names, types, offsets and the schema hashes
/// of the records it holds. A change to any of them is a new schema.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[repr(transparent)]
pub struct SchemaHash(pub ContentId);

/// A type whose value is its bytes.
///
/// # Safety
///
/// The implementor is `#[repr(C)]` (or a primitive) with no padding, no pointer, reference,
/// `Box` or `Arc` in it, and every bit pattern [`Record::check`] or [`Record::check_shallow`]
/// accepts is a valid value, as is every one when [`Record::SHALLOW_PLAIN`] holds: [`open`] and
/// [`Rel::get`] hand out `&Self` over bytes they have only checked with them. A struct is made
/// a record in safe code by [`record!`] ([`declared::Declared`]), whose one impl of this trait
/// checks the declaration against the struct.
pub unsafe trait Record: Copy + 'static {
    const SCHEMA: SchemaHash;
    /// Every `size_of::<Self>()` bytes are a valid value and hold no reference: `check`
    /// accepts anything at the right size and alignment, so a [`Slice`] of them is checked by
    /// its bounds alone, in O(1), however long it is.
    const PLAIN: bool = false;
    /// Every `size_of::<Self>()` bytes are a valid value once the [`Rel`]s and [`Slice`]s in it
    /// count as any bits (each is checked where it is followed): a view of one checks only
    /// where it is, so [`Slice::get`] of them is O(1), as for a [`Record::PLAIN`] one. `PLAIN`
    /// implies it; a record with a `bool` or an enum in its own bytes has it false.
    const SHALLOW_PLAIN: bool = Self::PLAIN;
    /// What must hold before any `Self` is viewed or its bytes read, evaluated at compile time
    /// where this crate does either: for a [`declared::Declared`] record, that its declaration
    /// is its type's; nothing for a record implemented by hand, whose `unsafe impl` promises it.
    const VERIFIED: () = ();
    /// Whether `bytes`, as for [`Record::check`], are a valid value, the [`Rel`]s and [`Slice`]s
    /// in it taken as they are and not followed: what [`Rel::get`] and [`Slice::get`] check of
    /// what they reach, the buffer having been checked whole by [`open`]. `check` accepts no
    /// more than this, so it is a sound default.
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        Self::check(bytes, buffer)
    }
    /// Whether a [`WordTable`] may be reachable from a value (`true` when it may, `false` only
    /// when it can't): [`Record::columns`] walks only what may hold one.
    const COLUMNS: bool = false;
    /// Whether `bytes`, `size_of::<Self>()` of them at an aligned address, are a valid value,
    /// the [`Rel`]s and [`Slice`]s in it within `buffer`.
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
    /// Pushes each [`WordTable`] reachable from the value at `bytes`, which [`Record::check`]
    /// accepted. What blaze-format's `tiers::BufferColumns` packs, codec by column.
    fn columns<O: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut O) {
        let _ = (bytes, buffer, out);
    }
}

/// The items of a [`Slice`] as a table of `u32` words: `rows` rows of `stride` words from
/// byte `at` of the buffer, one row an item. Each word of a row is a column of its own, so a
/// slice of records is packed field by field (a struct split into columns, as Parquet and
/// BtrBlocks split one). Words are moved bit for bit, so the bytes come back the same.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct WordTable {
    pub at: usize,
    pub rows: usize,
    pub stride: usize,
}

/// Where [`Record::columns`] puts the tables it finds: a vector of them, in the caller's
/// allocator.
pub trait WordTables {
    fn push_table(&mut self, table: WordTable);
}

impl<A: Allocator> WordTables for blaze_own::Vec<WordTable, A> {
    fn push_table(&mut self, table: WordTable) {
        self.push(table);
    }
}

#[cfg(feature = "alloc")]
impl<A: Allocator> WordTables for alloc::vec::Vec<WordTable, A> {
    fn push_table(&mut self, table: WordTable) {
        self.push(table);
    }
}

/// The words in a `T` when a [`Slice`] of them is a [`WordTable`], 0 when it is not: a `T`
/// a whole number of `u32` words long and at least as aligned as one, so each item starts on
/// a word. Its fields' meaning doesn't matter, only their bytes.
pub const fn row_words<T: Record>() -> usize {
    match size_of::<T>().is_multiple_of(4) && align_of::<T>() >= 4 {
        true => size_of::<T>() / 4,
        false => 0,
    }
}

/// The bytes of `value`, a record.
fn bytes_of<T: Record>(value: &T) -> &[u8] {
    raw::bytes_of(core::slice::from_ref(value))
}

/// The bytes of `values`, records, in place: what [`Writer::push_slice`] writes of them, so a
/// reader that wants a column as bytes (to hash it, to hand it on) takes no copy.
pub fn bytes_of_slice<T: Record>(values: &[T]) -> &[u8] {
    raw::bytes_of(values)
}

/// [`Record::columns`] of the field of type `F` at `offset` in `record`, a checked record.
pub fn field_columns<F: Record, O: WordTables + ?Sized>(
    record: &[u8],
    offset: usize,
    buffer: &[u8],
    out: &mut O,
) {
    if F::COLUMNS {
        F::columns(&record[offset..offset + size_of::<F>()], buffer, out);
    }
}

/// Checks the field of type `F` at `offset` in `record`, the bytes of a record in `buffer`:
/// what a [`Record::check`] calls for each of its fields.
pub fn check_field<F: Record>(
    record: &[u8],
    offset: usize,
    buffer: &[u8],
) -> Result<(), FormatError> {
    F::check(field_bytes::<F>(record, offset, buffer)?, buffer)
}

/// [`check_field`] by [`Record::check_shallow`]: what a [`Record::check_shallow`] calls for
/// each field that is not [`Record::SHALLOW_PLAIN`].
pub fn check_field_shallow<F: Record>(
    record: &[u8],
    offset: usize,
    buffer: &[u8],
) -> Result<(), FormatError> {
    let bytes = field_bytes::<F>(record, offset, buffer)?;
    match F::SHALLOW_PLAIN {
        true => Ok(()),
        false => F::check_shallow(bytes, buffer),
    }
}

/// The bytes of the field of type `F` at `offset` in `record`, inside it and aligned for `F`.
fn field_bytes<'a, F: Record>(
    record: &'a [u8],
    offset: usize,
    buffer: &[u8],
) -> Result<&'a [u8], FormatError> {
    let at = (record.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    let at = at + offset as u64;
    let Some(end) = offset.checked_add(size_of::<F>()).filter(|&end| end <= record.len()) else {
        return Err(FormatError::Invalid { at, what: "a field past the end of its record" });
    };
    let bytes = &record[offset..end];
    if !(bytes.as_ptr() as usize).is_multiple_of(align_of::<F>()) {
        return Err(FormatError::OutOfBounds { at });
    }
    Ok(bytes)
}

/// Whether `bytes` are where a `T` can be read: its size, at its alignment.
fn fits<T>(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
    let at = (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64;
    match bytes.len() == size_of::<T>() && (bytes.as_ptr() as usize).is_multiple_of(align_of::<T>())
    {
        true => Ok(()),
        false => Err(FormatError::Invalid { at, what: "a record of the wrong size or alignment" }),
    }
}

macro_rules! plain {
    ($($ty:ty => $name:literal),* $(,)?) => {$(
        // SAFETY: a primitive or a byte array, with no padding; every bit pattern is a value.
        unsafe impl Record for $ty {
            const SCHEMA: SchemaHash = schema($name);
            const PLAIN: bool = true;
            fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
                Ok(())
            }
        }
    )*};
}

plain! {
    u8 => "u8", u16 => "u16", u32 => "u32", u64 => "u64", u128 => "u128",
    i8 => "i8", i16 => "i16", i32 => "i32", i64 => "i64", i128 => "i128",
    ContentId => "ContentId", Fingerprint => "Fingerprint", SchemaHash => "SchemaHash",
}

// SAFETY: one byte, and `check` accepts only 0 and 1.
unsafe impl Record for bool {
    const SCHEMA: SchemaHash = schema("bool");
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        fits::<bool>(bytes, buffer)?;
        match bytes[0] {
            0 | 1 => Ok(()),
            _ => Err(FormatError::Invalid {
                at: (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64,
                what: "a bool that is neither 0 nor 1",
            }),
        }
    }
}

// SAFETY: an array has no padding between its elements, each checked as a `T`.
unsafe impl<T: Record, const N: usize> Record for [T; N] {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("array"), Part::U64(N as u64), Part::Schema(T::SCHEMA)]);
    const PLAIN: bool = T::PLAIN;
    const SHALLOW_PLAIN: bool = T::SHALLOW_PLAIN;
    const COLUMNS: bool = T::COLUMNS;
    const VERIFIED: () = T::VERIFIED;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        if T::PLAIN {
            return fits::<[T; N]>(bytes, buffer);
        }
        (0..N).try_for_each(|i| check_field::<T>(bytes, i * size_of::<T>(), buffer))
    }
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        if T::SHALLOW_PLAIN {
            return fits::<[T; N]>(bytes, buffer);
        }
        (0..N).try_for_each(|i| check_field_shallow::<T>(bytes, i * size_of::<T>(), buffer))
    }
    fn columns<O: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut O) {
        (0..N).for_each(|i| field_columns::<T, O>(bytes, i * size_of::<T>(), buffer, out));
    }
}

/// A reference to a `T` elsewhere in the same buffer: its offset from this field's own
/// address, in bytes, an `O` ([`Offset`]: `i32`, or `i16` or `i8` for a target close behind).
/// 0 is no `T`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Rel<T, O: Offset = i32> {
    pub offset: O,
    of: PhantomData<fn() -> T>,
}

/// `len` `T`s in a row elsewhere in the same buffer, starting `offset` bytes from this field:
/// an `i32` and a `u32` by default, an `i16` and a `u16` or an `i8` and a `u8` ([`Offset`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Slice<T, O: Offset = i32> {
    pub offset: O,
    pub len: O::Len,
    of: PhantomData<fn() -> T>,
}

/// Bytes in the buffer, read as text by [`Slice::as_str`], which checks they are UTF-8 on each
/// read; a [`Text`] is checked once, by [`open`].
pub type Str = Slice<u8>;

/// Where `field` is in `buffer`, from its start.
fn position<F>(field: &F, buffer: &[u8]) -> Result<u64, FormatError> {
    let (address, base) = (field as *const F as usize, buffer.as_ptr() as usize);
    let at = address.wrapping_sub(base);
    match address >= base && at.checked_add(size_of::<F>()).is_some_and(|end| end <= buffer.len()) {
        true => Ok(at as u64),
        false => Err(FormatError::OutOfBounds { at: at as u64 }),
    }
}

/// Where `count` `T`s that a field at `field_at` points `offset` bytes away from start, from
/// the buffer's start: after the header, before the field, aligned for `T`. What [`open`]'s
/// walk checks of every reference it reaches, so a [`Verified`] reads them with no check.
fn target<T>(field_at: u64, offset: i64, count: u64, buffer: &[u8]) -> Result<usize, FormatError> {
    let bad = FormatError::OutOfBounds { at: field_at };
    let target = field_at as i64 + offset;
    if target < HEADER_LEN as i64 {
        return Err(bad);
    }
    let target = target as u64;
    let len =
        (size_of::<T>() as u64).checked_mul(count).ok_or(FormatError::OutOfBounds { at: field_at })?;
    // The bound a [`Verified`] relies on to read a target inside the bytes before its field.
    let bound = match planted::forward() {
        false => field_at,
        true => buffer.len() as u64,
    };
    if target.checked_add(len).is_none_or(|end| end > bound) {
        return Err(bad);
    }
    let address = (buffer.as_ptr() as usize).wrapping_add(target as usize);
    match address.is_multiple_of(align_of::<T>()) {
        true => Ok(target as usize),
        false => Err(bad),
    }
}

impl<T, O: Offset> Rel<T, O> {
    pub const NONE: Rel<T, O> = Rel { offset: O::ZERO, of: PhantomData };

    /// The reference a field at `field_at` holds to a `T` at `target_at`, both from the
    /// buffer's start: the `T` was written before the record holding the field. Refused when
    /// it was not, or when the distance does not fit an `O`: the form for a narrow offset
    /// (an `i32` one is [`Rel::between`]).
    pub fn between_as(field_at: u64, target_at: u64) -> Result<Rel<T, O>, Unfit> {
        let offset = offset::offset_between::<O>(field_at, target_at, true)?;
        Ok(Rel { offset, of: PhantomData })
    }

    pub fn is_none(&self) -> bool {
        self.offset == O::ZERO
    }
}

impl<T> Rel<T> {
    /// [`Rel::between_as`] for the default `i32` offset, which reaches across any buffer under
    /// 2 GiB: a plain value, as every writer of a record makes one. Panics, naming the
    /// [`Unfit`], when the `T` was not written before the field or the buffer is 2 GiB or
    /// more, as it always did.
    pub fn between(field_at: u64, target_at: u64) -> Rel<T> {
        Rel::between_as(field_at, target_at).unwrap_or_else(|unfit| panic!("{unfit}"))
    }
}

impl<T: Record, O: Offset> Rel<T, O> {
    /// The `T` this points at, `None` for [`Rel::NONE`]. Panics when `buffer` is not the
    /// buffer this field is in, or the `T` is not a valid one there: a buffer from [`open`]
    /// was checked whole, so this checks again only what a reference itself holds.
    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> Option<&'a T> {
        self.follow(buffer).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The `T` this points at, checked where it is and by [`Record::check_shallow`]: the
    /// references in it are checked when they are followed in turn, so following each
    /// reference of a buffer once costs what [`open`] did, not that times the depth.
    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<Option<&'a T>, FormatError> {
        self.follow_as::<false>(buffer)
    }

    /// [`Rel::follow`], the `T` checked with all it reaches when `DEEP`: [`Record::check`].
    fn follow_as<'a, const DEEP: bool>(
        &'a self,
        buffer: &'a [u8],
    ) -> Result<Option<&'a T>, FormatError> {
        if self.offset == O::ZERO {
            return Ok(None);
        }
        let at = position(self, buffer)?;
        let target = target::<T>(at, self.offset.distance(), 1, buffer)?;
        raw::view::<T, DEEP>(&buffer[target..target + size_of::<T>()], buffer).map(Some)
    }
}

impl<T, O: Offset> Slice<T, O> {
    pub const EMPTY: Slice<T, O> = Slice { offset: O::ZERO, len: O::LEN_ZERO, of: PhantomData };

    /// The slice a field at `field_at` holds of `len` `T`s at `target_at`, both from the
    /// buffer's start: the `T`s were written before the record holding the field. Refused
    /// when they were not, or when the distance or the length does not fit an `O`: the form
    /// for a narrow offset (an `i32` one is [`Slice::between`]).
    pub fn between_as(field_at: u64, target_at: u64, len: usize) -> Result<Slice<T, O>, Unfit> {
        let long = Unfit::Long { field_at, len: len as u64, offset: O::NAME };
        let len = O::of_count(len as u64).ok_or(long)?;
        let offset = offset::offset_between::<O>(field_at, target_at, false)?;
        Ok(Slice { offset, len, of: PhantomData })
    }

    /// How many `T`s.
    pub fn count(&self) -> usize {
        O::count(self.len) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == O::LEN_ZERO
    }
}

impl<T> Slice<T> {
    /// [`Slice::between_as`] for the default `i32` offset: a plain value, as [`Rel::between`]
    /// is. Panics, naming the [`Unfit`], when the `T`s were not written before the field, or
    /// the buffer is 2 GiB or more, or they are 2^32 or more, as it always did.
    pub fn between(field_at: u64, target_at: u64, len: usize) -> Slice<T> {
        Slice::between_as(field_at, target_at, len).unwrap_or_else(|unfit| panic!("{unfit}"))
    }
}

impl<T: Record, O: Offset> Slice<T, O> {
    /// The `T`s this points at. Panics as [`Rel::get`] does.
    pub fn get<'a>(&'a self, buffer: &'a [u8]) -> &'a [T] {
        self.follow(buffer).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The `T`s this points at, checked as [`Rel::follow`] checks its `T`: in O(1) for a
    /// [`Record::SHALLOW_PLAIN`] `T`, however long the slice.
    pub fn follow<'a>(&'a self, buffer: &'a [u8]) -> Result<&'a [T], FormatError> {
        self.follow_as::<false>(buffer)
    }

    /// [`Slice::follow`], each `T` checked with all it reaches when `DEEP`: [`Record::check`].
    fn follow_as<'a, const DEEP: bool>(&'a self, buffer: &'a [u8]) -> Result<&'a [T], FormatError> {
        if self.is_empty() {
            return Ok(&[]);
        }
        let at = position(self, buffer)?;
        let count = O::count(self.len);
        let target = target::<T>(at, self.offset.distance(), count, buffer)?;
        let len = count as usize;
        raw::view_slice::<T, DEEP>(&buffer[target..target + size_of::<T>() * len], len, buffer)
    }
}

impl<O: Offset> Slice<u8, O> {
    /// The text this points at, refused when it is not UTF-8.
    pub fn as_str<'a>(&'a self, buffer: &'a [u8]) -> Result<&'a str, FormatError> {
        let bytes = self.follow(buffer)?;
        utf8(bytes, buffer)
    }
}

/// `bytes`, a part of `buffer`, as text, refused when it is not UTF-8.
fn utf8<'a>(bytes: &'a [u8], buffer: &[u8]) -> Result<&'a str, FormatError> {
    core::str::from_utf8(bytes).map_err(|_| FormatError::Invalid {
        at: (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64,
        what: "text that is not UTF-8",
    })
}

/// The schema of a reference named `name` to `of`: the default width adds no part, so a
/// record of `i32` references keeps the hash it had before widths existed; another width adds
/// its name.
const fn reference_schema<O: Offset>(name: &str, of: SchemaHash) -> SchemaHash {
    match O::WIDE {
        true => schema_of(&[Part::Str(name), Part::Schema(of)]),
        false => schema_of(&[Part::Str(name), Part::Str(O::NAME), Part::Schema(of)]),
    }
}

// SAFETY: an `O` (an integer) and a zero-sized marker: every bit pattern is a `Rel`; `check`
// follows it and checks the `T` it reaches.
unsafe impl<T: Record, O: Offset> Record for Rel<T, O> {
    const SCHEMA: SchemaHash = reference_schema::<O>("Rel", T::SCHEMA);
    const SHALLOW_PLAIN: bool = true;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        raw::in_place::<Rel<T, O>>(bytes, buffer)?.follow_as::<true>(buffer).map(|_| ())
    }
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        raw::in_place::<Rel<T, O>>(bytes, buffer).map(|_| ())
    }
    const COLUMNS: bool = !T::PLAIN;
    fn columns<W: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut W) {
        let rel = raw::in_place::<Rel<T, O>>(bytes, buffer).expect("a checked Rel");
        if let Some(target) = rel.get(buffer) {
            T::columns(bytes_of(target), buffer, out);
        }
    }
}

// SAFETY: an `O` and an `O::Len` (integers of one width, so no padding) and a zero-sized
// marker: every bit pattern is a `Slice`; `check` follows it and checks each `T` it reaches.
unsafe impl<T: Record, O: Offset> Record for Slice<T, O> {
    const SCHEMA: SchemaHash = reference_schema::<O>("Slice", T::SCHEMA);
    const SHALLOW_PLAIN: bool = true;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        raw::in_place::<Slice<T, O>>(bytes, buffer)?.follow_as::<true>(buffer).map(|_| ())
    }
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        raw::in_place::<Slice<T, O>>(bytes, buffer).map(|_| ())
    }
    // `!T::PLAIN`, not `T::COLUMNS`: a record may hold slices of itself, and `PLAIN` never
    // refers back to the type.
    const COLUMNS: bool = row_words::<T>() > 0 || !T::PLAIN;
    fn columns<W: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut W) {
        let slice = raw::in_place::<Slice<T, O>>(bytes, buffer).expect("a checked Slice");
        let items = slice.get(buffer);
        if row_words::<T>() > 0 && !items.is_empty() {
            let at = (items.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize);
            out.push_table(WordTable { at, rows: items.len(), stride: row_words::<T>() });
        }
        if !T::PLAIN {
            items.iter().for_each(|item| T::columns(bytes_of(item), buffer, out));
        }
    }
}

/// What a buffer starts with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Header {
    /// `b"BLAZEV2\0"`.
    pub magic: [u8; 8],
    pub schema: SchemaHash,
    /// The whole buffer's length, this header included.
    pub len: u64,
    /// Where the root record is, from the buffer's start.
    pub root: u64,
}

pub const MAGIC: [u8; 8] = *b"BLAZEV2\0";

/// The alignment every buffer is mapped and written at: records align to at most this.
pub const ALIGN: usize = 16;

/// How long a [`Header`] is: no record is in the first this many bytes of a buffer.
pub const HEADER_LEN: usize = size_of::<Header>();

const _: () = assert!(HEADER_LEN == 8 + 32 + 8 + 8, "the header has padding");

fn read_u64(buffer: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&buffer[at..at + 8]);
    u64::from_le_bytes(word)
}

/// The header of `buffer`, checked: its magic, its length, and that the buffer is at an
/// address aligned to [`ALIGN`], which every record in it relies on.
pub fn header(buffer: &[u8]) -> Result<Header, FormatError> {
    if buffer.len() < HEADER_LEN {
        return Err(FormatError::Header("shorter than a header"));
    }
    if !(buffer.as_ptr() as usize).is_multiple_of(ALIGN) {
        return Err(FormatError::Header("not at an address aligned to 16 bytes"));
    }
    let mut magic = [0; 8];
    magic.copy_from_slice(&buffer[..8]);
    if magic != MAGIC {
        return Err(FormatError::Header("a wrong magic"));
    }
    let mut schema = [0; 32];
    schema.copy_from_slice(&buffer[offset_of!(Header, schema)..offset_of!(Header, schema) + 32]);
    let header = Header {
        magic,
        schema: SchemaHash(ContentId(schema)),
        len: read_u64(buffer, offset_of!(Header, len)),
        root: read_u64(buffer, offset_of!(Header, root)),
    };
    if header.len != buffer.len() as u64 {
        return Err(FormatError::Header("a length that is not the buffer's"));
    }
    Ok(header)
}

/// The root `T` of `buffer`, after checking its header, its schema and every record reached
/// from the root: a [`Verified`], whose reads check nothing again. It derefs to the `T`.
pub fn open<T: Record>(buffer: &[u8]) -> Result<&Verified<T>, FormatError> {
    open_as::<T>(buffer, T::SCHEMA)
}

/// The root `R` of a buffer of schema `schema`, checked as [`open`] checks: what a
/// `Persist::read` opens its buffer with, the buffer's schema being the `Persist` type's.
pub fn open_as<R: Record>(buffer: &[u8], schema: SchemaHash) -> Result<&Verified<R>, FormatError> {
    let header = header(buffer)?;
    if header.schema != schema {
        return Err(FormatError::Schema { found: header.schema, wanted: schema });
    }
    record_at::<R>(buffer, header.root)
}

/// The root `R` of `buffer` with no check at all, in O(1): what a reader holding the proof of an
/// earlier [`open`] or [`open_as`] of the same bytes uses to view them again (D114 rule 1: a
/// record is checked once a process and read in place after; rule 74).
///
/// # Safety
///
/// [`open`] or [`open_as`] for `R` returned `Ok` for these same bytes, at this same address,
/// earlier in this process, and they have not changed since.
pub unsafe fn reopen<R: Record>(buffer: &[u8]) -> &Verified<R> {
    let start = read_u64(buffer, offset_of!(Header, root)) as usize;
    // SAFETY: `record_at::<R>` accepted the `R` at the header's root offset of these unchanged
    // bytes (the caller's promise): `start + size_of::<R>()` is inside the buffer, the `R` is
    // aligned for it and deeply checked, so `Verified`'s invariant holds for the buffer's prefix
    // up to the `R`'s end, the slice `record_at` gave `Verified::assume`.
    unsafe { Verified::assume(buffer.get_unchecked(..start + size_of::<R>())) }
}

/// The `R` at `at` in `buffer`, checked: past the header, inside the buffer, aligned for `R`,
/// and accepted by [`Record::check`], which follows every reference it reaches. A record found
/// by an offset of the caller's (a table placed beside the root); [`open_as`] reads the root
/// with it. The one place a [`Verified`] is made from bytes.
pub fn record_at<R: Record>(buffer: &[u8], at: u64) -> Result<&Verified<R>, FormatError> {
    let bad = || FormatError::OutOfBounds { at };
    let start = usize::try_from(at).map_err(|_| bad())?;
    let end = start
        .checked_add(size_of::<R>())
        .filter(|&end| end <= buffer.len() && start >= HEADER_LEN)
        .ok_or_else(bad)?;
    if !(buffer.as_ptr() as usize).wrapping_add(start).is_multiple_of(align_of::<R>()) {
        return Err(bad());
    }
    raw::view::<R, true>(&buffer[start..end], buffer)?;
    // SAFETY: `Verified`'s invariant, for the buffer's bytes up to the `R`'s end: they start at
    // the start of `buffer`, the `R` is their last `size_of::<R>()` bytes, aligned for it
    // (checked above), and `R::check` (the deep one, `view::<R, true>`) accepted it against
    // `buffer` with every reference it reaches, each target before its field and so inside
    // these bytes.
    Ok(unsafe { Verified::assume(&buffer[..end]) })
}

/// The domain of a buffer's content ID ([`buffer_id`]).
pub const BUFFER_DOMAIN: &str = "blaze v2 buffer 1";

/// The content ID of a whole buffer, its header included: what the store and the daemon's
/// image key a buffer by, and [`open_verified`] checks it against.
pub fn buffer_id(buffer: &[u8]) -> ContentId {
    ContentId::of(BUFFER_DOMAIN, buffer)
}

/// [`open_as`] of bytes from outside this process (the store, a pack, the image, the
/// network), after checking they are the buffer `key` names. A decoder that trusts what a
/// record holds beyond its bounds (a query ID, an interned handle) is only given bytes
/// opened this way.
pub fn open_verified<R: Record>(
    buffer: &[u8],
    schema: SchemaHash,
    key: ContentId,
) -> Result<&Verified<R>, FormatError> {
    verify(buffer, key)?;
    open_as::<R>(buffer, schema)
}

fn verify(buffer: &[u8], key: ContentId) -> Result<(), FormatError> {
    let found = buffer_id(buffer);
    match found == key {
        true => Ok(()),
        false => Err(FormatError::Hash { found, wanted: key }),
    }
}

record! {
    /// A span in a span side table (docs/v2/plan/design/data.md section 5, "Spans"): the anchor
    /// is the content ID of the item the span is relative to, `index` its place in that item's
    /// table. Never hashed into code: a formatting edit changes the table, not the code that
    /// names it.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
    pub struct SpanRef: 1 {
        pub anchor: ContentId,
        pub index: u32,
    }
}

const _: () = assert!(size_of::<SpanRef>() == 32 + 4, "a SpanRef has padding");

record! {
    /// A span of an anchor's span table: bytes from the anchor's start, so an edit that moves
    /// the anchor in its file moves none of its spans.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct RelSpan: 1 {
        pub start: u32,
        pub len: u32,
    }
}

/// Why bytes are not a value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// The buffer is of another schema: made by another blaze, or of another type.
    Schema { found: SchemaHash, wanted: SchemaHash },
    /// Not a buffer: a wrong magic, a length that is not the buffer's.
    Header(&'static str),
    /// A record at `at` that is not a valid value: `what` says why.
    Invalid { at: u64, what: &'static str },
    /// A length or index past what bounds it: `at` is the buffer's byte the value is at (the
    /// word or field holding it, or the record holding it where the reader has no finer
    /// place), `what` names the field and the bound (a list at most as long as the bytes left
    /// can hold, an index below its table's length), `found` is the value read and `bound` the
    /// bound's value.
    Bound { at: u64, what: &'static str, found: u64, bound: u64 },
    /// An ID word at `at` that is 0, which names nothing: `what` names the field.
    Zero { at: u64, what: &'static str },
    /// A reference at `at` that leaves the buffer or is misaligned.
    OutOfBounds { at: u64 },
    /// Bytes that are not the buffer their key names ([`open_verified`]).
    Hash { found: ContentId, wanted: ContentId },
    /// A cold column whose frame, at byte `at`, is not a standard zstd frame: `found` is what it
    /// starts with (a legacy or skippable frame's magic, or garbage), never handed to zstd.
    Frame { at: u64, found: [u8; 4] },
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatError::Schema { found, wanted } => {
                write!(f, "a buffer of schema {} where {} was wanted", found.0, wanted.0)
            }
            FormatError::Header(what) => write!(f, "not a blaze buffer: {what}"),
            FormatError::Invalid { at, what } => write!(f, "invalid record at byte {at}: {what}"),
            FormatError::Bound { at, what, found, bound } => {
                write!(
                    f,
                    "invalid record at byte {at}: {what}: found {found}, past its bound {bound}"
                )
            }
            FormatError::Zero { at, what } => {
                write!(f, "invalid record at byte {at}: {what} is 0, which names nothing")
            }
            FormatError::OutOfBounds { at } => {
                write!(f, "a reference at byte {at} leaves the buffer or is misaligned")
            }
            FormatError::Hash { found, wanted } => {
                write!(f, "a buffer of content {found} where {wanted} was wanted")
            }
            FormatError::Frame { at, found } => {
                write!(
                    f,
                    "a cold column's frame at byte {at} starts with {found:02x?}, not zstd's magic [28, b5, 2f, fd]"
                )
            }
        }
    }
}

impl core::error::Error for FormatError {}
