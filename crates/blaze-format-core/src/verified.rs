//! [`Verified`]: the proof that [`crate::open`] checked a record with all it reaches, so every
//! read through it checks nothing again (rule 74, verify-once), and [`Text`], the text a
//! buffer's walk checks once.
//!
//! # The proof
//!
//! A `&'a Verified<T>` is a slice of the buffer `open` checked: from the buffer's start to the
//! end of one `T` in it, the `T` being its last `size_of::<T>()` bytes. What it promises (the
//! invariant, which every unsafe read below relies on and every constructor establishes):
//!
//! 1. its bytes start where the checked buffer starts, so a byte's place in them is its place
//!    in the buffer, the one [`crate::Record::check`] computed every reference from;
//! 2. the `T` (its last `size_of::<T>()` bytes) is at an address aligned for `T`;
//! 3. `T::check`, the deep one, accepted the `T` against the buffer: by `Record`'s contract a
//!    valid `T`, and for each kind of value the format defines, what it reaches:
//!    - a [`Rel`]: 0, or a target after the header, aligned for its `T`, ending at or before
//!      the field (every reference points back), itself accepted by the deep check;
//!    - a [`Slice`]: no items, or `len` items after the header, the first aligned for `T`
//!      (and so each), ending at or before the field, each accepted by the deep check;
//!    - a [`Text`]: a `Slice<u8>` as above whose bytes are UTF-8;
//!    - an array: each element accepted by the deep check at its place;
//!    - a declared record ([`crate::declared::Declared`]): each field accepted by the deep
//!      check at its offset (`declared`'s layout checks every field, reflection having checked
//!      the declaration against the struct), a `bool` 0 or 1, a byte enum one of its bytes.
//!
//! Since every target ends at or before its field, everything the `T` reaches lies inside
//! the proof's bytes, which is why the proof is the buffer up to the `T` and not the `T` alone:
//! a read derives its pointer from those bytes, never past them. So a target, an item and a
//! field are again a `Verified` of a shorter prefix of the same bytes, with the same promise:
//! (1) holds as the start is the same; (2) by the alignment checked; (3) by the deep check
//! having recursed into exactly that target, item or field.
//!
//! A proof is made only by [`crate::record_at`] (and so [`crate::open`], [`crate::open_as`],
//! [`crate::open_verified`]) after its deep check, and by the projections here from another
//! proof; [`Verified::assume`] is private to this crate and `unsafe`. A projection to a field
//! names it by its offset, checked at compile time by reflection to be a field of that type
//! ([`Verified::field`], the [`crate::field!`] macro), never by a reference the caller hands
//! in: safe code can make a `&Rel` inside the buffer that no check covered (the buffer's own
//! bytes viewed as records it built), so a proof taken from a caller's reference would be a
//! promise nothing kept.

use core::fmt;
use core::marker::PhantomData;
use core::mem::size_of;
use core::mem::type_info::{self, Type, TypeKind};
use core::ops::Deref;

use crate::declared::Declared;
use crate::offset::Unfit;
use crate::{FormatError, Offset, Part, Record, Rel, SchemaHash, Slice, raw, schema_of};

/// A `T` that [`crate::open`] checked with everything it reaches: reads through it check
/// nothing. It derefs to the `T`; [`Verified::target`], [`Verified::items`],
/// [`Verified::as_str`], [`Verified::index`] and [`Verified::field`] read what it holds as
/// proofs in turn, with no bound, alignment or value check and no `Result`. The module's
/// comment states the invariant.
#[repr(transparent)]
pub struct Verified<T> {
    of: PhantomData<fn() -> T>,
    /// The checked buffer from its start to the `T`'s end. Named so no record's field is
    /// hidden by it (a field access through the deref finds this first inside this crate).
    checked_bytes: [u8],
}

impl<T: Record> Verified<T> {
    /// `prefix` as a proof of the `T` it ends with.
    ///
    /// # Safety
    ///
    /// `prefix` holds the module's invariant for `T`: it starts at the start of a buffer that
    /// `T::check` (the deep one) accepted its last `size_of::<T>()` bytes against, and those
    /// bytes are at an address aligned for `T`.
    #[inline(always)]
    pub(crate) unsafe fn assume(prefix: &[u8]) -> &Verified<T> {
        const { T::VERIFIED };
        debug_assert!(prefix.len() >= size_of::<T>());
        // SAFETY: `Verified<T>` is `#[repr(transparent)]` over `[u8]` (its other field is a
        // zero-sized marker), so a pointer to `[u8]` is one to it with the same length; the
        // reference borrows `prefix`.
        unsafe { &*(prefix as *const [u8] as *const Verified<T>) }
    }

    /// The `T`, borrowed for as long as the buffer is. An associated function, not a method,
    /// as the proof derefs to the `T` and must hide none of its methods (as `Box::leak`).
    #[inline(always)]
    pub fn get(this: &Verified<T>) -> &T {
        const { T::VERIFIED };
        let at = Verified::at(this);
        // SAFETY: the invariant: the last `size_of::<T>()` bytes are a `T` at an address
        // aligned for it that `T::check` accepted, which `Record`'s contract makes a valid
        // `T`; the pointer is derived from the proof's bytes, which it lies inside.
        unsafe { &*this.checked_bytes.as_ptr().add(at).cast::<T>() }
    }

    /// Where the `T` is, from the buffer's start.
    #[inline(always)]
    pub fn at(this: &Verified<T>) -> usize {
        this.checked_bytes.len() - size_of::<T>()
    }

    /// The checked buffer from its start to the `T`'s end: the bytes everything the `T`
    /// reaches is in. A reader that still follows by [`Rel::follow`] gives these.
    #[inline(always)]
    pub fn bytes(this: &Verified<T>) -> &[u8] {
        &this.checked_bytes
    }

    /// The proof of the `U` that ends `end` bytes into this one's bytes.
    ///
    /// # Safety
    ///
    /// `end <= self.checked_bytes.len()`, and the `U` that ends there holds the module's
    /// invariant: at an address aligned for it, accepted by the deep check that accepted this
    /// `T`.
    #[inline(always)]
    unsafe fn within<U: Record>(&self, end: usize) -> &Verified<U> {
        // SAFETY: `end` is inside the bytes, the caller says.
        let prefix = unsafe { self.checked_bytes.get_unchecked(..end) };
        // SAFETY: the prefix starts where this proof's bytes start, the buffer's start (the
        // invariant's first point), and the caller says the `U` ending it holds the rest.
        unsafe { Verified::assume(prefix) }
    }
}

impl<T: Record> Deref for Verified<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        Verified::get(self)
    }
}

impl<T: Record + fmt::Debug> fmt::Debug for Verified<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Verified::get(self).fmt(f)
    }
}

impl<T: Record + PartialEq> PartialEq for Verified<T> {
    fn eq(&self, other: &Verified<T>) -> bool {
        Verified::get(self) == Verified::get(other)
    }
}

impl<T: Record + Eq> Eq for Verified<T> {}

impl<T: Record + PartialEq> PartialEq<T> for Verified<T> {
    fn eq(&self, other: &T) -> bool {
        Verified::get(self) == other
    }
}

impl<T: Record, O: Offset> Verified<Rel<T, O>> {
    /// The `T` this points at, `None` for [`Rel::NONE`]: checked by the walk, so read here
    /// with no check.
    #[inline]
    pub fn target(&self) -> Option<&Verified<T>> {
        let offset = Verified::get(self).offset;
        if offset == O::ZERO {
            return None;
        }
        // The walk checked `HEADER_LEN <= target` and `target + size_of::<T>() <= field`, so
        // neither sum leaves `0..=Verified::at(self)`.
        let target = (Verified::at(self) as i64 + offset.distance()) as usize;
        // SAFETY: the invariant for a `Rel` (the module's comment): a nonzero offset was checked
        // to reach a `T` aligned for it, ending at or before this field and so inside these
        // bytes, which the deep check accepted.
        Some(unsafe { self.within::<T>(target + size_of::<T>()) })
    }
}

impl<T: Record, O: Offset> Verified<Slice<T, O>> {
    /// The `T`s this points at, checked by the walk, so read here with no check.
    #[inline]
    pub fn items(&self) -> Items<'_, T> {
        let slice = Verified::get(self);
        if slice.is_empty() {
            return Items { bytes: &[], len: 0, of: PhantomData };
        }
        let len = O::count(slice.len) as usize;
        let start = (Verified::at(self) as i64 + slice.offset.distance()) as usize;
        let end = start + len * size_of::<T>();
        // SAFETY: the invariant for a `Slice`: `len` `T`s from the target, ending at or before
        // this field and so inside these bytes (`end` is in range); `Items` holds them as its
        // last `len * size_of::<T>()` bytes, each aligned and accepted by the deep check.
        let bytes = unsafe { self.checked_bytes.get_unchecked(..end) };
        Items { bytes, len, of: PhantomData }
    }
}

impl<T: Record, const N: usize> Verified<[T; N]> {
    /// Element `index`, `None` past the end.
    #[inline]
    pub fn index(&self, index: usize) -> Option<&Verified<T>> {
        match index < N {
            // SAFETY: element `index` ends `(index + 1) * size_of::<T>()` bytes into the array,
            // inside it; an array's elements are each aligned as it is (a size is a multiple of
            // its alignment), and its deep check accepted each at its place.
            true => Some(unsafe {
                self.within::<T>(Verified::at(self) + (index + 1) * size_of::<T>())
            }),
            false => None,
        }
    }
}

impl<R: Declared> Verified<R> {
    /// The field of type `F` at byte `OFFSET` of the record, as a proof: what the
    /// [`crate::field!`] macro calls, naming the field. `ty` only names `F` and is never
    /// called; the field is found by `OFFSET`, which reflection checks at compile time is a
    /// field of `R` of type `F`, so no reference of the caller's stands for it.
    /// An associated function, as [`Verified::get`] is.
    #[inline(always)]
    pub fn field<F: Record, const OFFSET: usize>(
        this: &Verified<R>,
        ty: fn(&R) -> &F,
    ) -> &Verified<F> {
        let _ = ty;
        const { R::VERIFIED };
        const { FieldAt::<R, F, OFFSET>::FOUND.assert() };
        // SAFETY: reflection found a field of `R` of type `F` at `OFFSET`, so it ends
        // `OFFSET + size_of::<F>()` bytes into the record, inside it, aligned for `F` (a
        // field of a `#[repr(C)]` struct at its aligned address); a declared record's deep
        // check (its verdict asserted by `R::VERIFIED` above) checks every field at its offset
        // by its own deep check.
        unsafe { this.within::<F>(Verified::at(this) + OFFSET + size_of::<F>()) }
    }
}

/// Whether a struct `S` has a field of type `F` at byte `OFFSET`, read by reflection: what
/// [`Verified::field`] asserts at compile time. A constant, as reflection's calls are made only
/// at compile time.
struct FieldAt<S, F, const OFFSET: usize>(PhantomData<(S, F)>);

/// What reflection says of a field asked for by offset.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Found {
    Field,
    NotAStruct,
    NoField,
}

impl Found {
    const fn assert(self) {
        match self {
            Found::Field => {}
            Found::NotAStruct => panic!("a field of a record that is not a struct"),
            Found::NoField => {
                panic!("no field of the record has this type at this offset: name it by `field!`")
            }
        }
    }
}

impl<S: 'static, F: 'static, const OFFSET: usize> FieldAt<S, F, OFFSET> {
    const FOUND: Found = match Type::of::<S>().kind {
        TypeKind::Struct => {
            let id = type_info::of::<S>();
            let (count, mut index, mut found) = (id.fields(0), 0, Found::NoField);
            while index < count {
                let field = id.field(0, index);
                if field.offset() == OFFSET && field.type_id() == type_info::of::<F>() {
                    found = Found::Field;
                }
                index += 1;
            }
            found
        }
        _ => Found::NotAStruct,
    };
}

/// The field `$field` of `$verified`, a `&Verified<$record>`, as a `&Verified` of its type:
/// [`Verified::field`] at the field's offset.
///
/// ```ignore
/// let root = blaze_format::open::<Node>(&buffer)?;
/// let children = blaze_format::field!(root, Node, children).items();
/// ```
#[macro_export]
macro_rules! field {
    ($verified:expr, $record:ty, $field:ident) => {
        $crate::Verified::<$record>::field::<_, { ::core::mem::offset_of!($record, $field) }>(
            $verified,
            |record: &$record| &record.$field,
        )
    };
}

/// The items of a [`Slice`] a [`Verified`] holds, each a proof in turn.
#[derive(Clone, Copy)]
pub struct Items<'a, T> {
    /// The checked buffer from its start to the last item's end.
    bytes: &'a [u8],
    len: usize,
    of: PhantomData<fn() -> T>,
}

impl<'a, T: Record> Items<'a, T> {
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Where the first item is, from the buffer's start.
    #[inline(always)]
    fn start(&self) -> usize {
        self.bytes.len() - self.len * size_of::<T>()
    }

    /// The items as a slice of `T`s.
    #[inline]
    pub fn as_slice(&self) -> &'a [T] {
        const { T::VERIFIED };
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the invariant for a `Slice`: the last `len * size_of::<T>()` bytes are `len`
        // `T`s, the first aligned for `T` (and so each), each accepted by the deep check and so
        // a valid `T`; the pointer is derived from the proof's bytes, which they lie inside.
        unsafe {
            core::slice::from_raw_parts(self.bytes.as_ptr().add(self.start()).cast::<T>(), self.len)
        }
    }

    /// Item `index` as a proof, `None` past the end.
    #[inline]
    pub fn get(&self, index: usize) -> Option<&'a Verified<T>> {
        match index < self.len {
            true => {
                let end = self.start() + (index + 1) * size_of::<T>();
                // SAFETY: item `index` ends at `end`, inside the bytes; it is aligned and was
                // accepted by the deep check (the invariant for a `Slice`), and the prefix
                // starts at the buffer's start.
                Some(unsafe { Verified::assume(self.bytes.get_unchecked(..end)) })
            }
            false => None,
        }
    }

    /// Each item as a proof, in order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &'a Verified<T>> + 'a {
        let items = *self;
        (0..items.len).map(move |index| items.get(index).expect("an index below the length"))
    }
}

impl<T: Record + fmt::Debug> fmt::Debug for Items<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.as_slice()).finish()
    }
}

/// UTF-8 text elsewhere in the same buffer: a [`Slice`] of bytes that [`crate::open`]'s walk
/// checks is UTF-8, so a [`Verified`] reads it as a `&str` with no check. A [`crate::Str`] is
/// checked on every read instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(transparent)]
pub struct Text<O: Offset = i32> {
    pub bytes: Slice<u8, O>,
}

impl<O: Offset> Text<O> {
    pub const EMPTY: Text<O> = Text { bytes: Slice::EMPTY };

    /// The text a field at `field_at` holds of `len` bytes at `target_at`: as
    /// [`Slice::between_as`], written by [`crate::Writer::push_str`]: the form for a narrow
    /// offset (an `i32` one is [`Text::between`]).
    pub fn between_as(field_at: u64, target_at: u64, len: usize) -> Result<Text<O>, Unfit> {
        Slice::between_as(field_at, target_at, len).map(|bytes| Text { bytes })
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The text, checked where it is and that it is UTF-8: for bytes no [`crate::open`] checked.
    pub fn as_str<'a>(&'a self, buffer: &'a [u8]) -> Result<&'a str, FormatError> {
        self.bytes.as_str(buffer)
    }
}

impl Text {
    /// [`Text::between_as`] for the default `i32` offset: a plain value; panics as
    /// [`Slice::between`] does.
    pub fn between(field_at: u64, target_at: u64, len: usize) -> Text {
        Text { bytes: Slice::between(field_at, target_at, len) }
    }
}

// SAFETY: `#[repr(transparent)]` over a `Slice<u8, O>`, every bit pattern of which is one;
// `check` follows it, and accepts only UTF-8.
unsafe impl<O: Offset> Record for Text<O> {
    const SCHEMA: SchemaHash =
        schema_of(&[Part::Str("Text"), Part::Schema(<Slice<u8, O> as Record>::SCHEMA)]);
    const SHALLOW_PLAIN: bool = true;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        let text = raw::in_place::<Text<O>>(bytes, buffer)?;
        <Slice<u8, O> as Record>::check(bytes, buffer)?;
        match crate::planted::not_utf8() {
            false => text.as_str(buffer).map(|_| ()),
            true => Ok(()),
        }
    }
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        raw::in_place::<Text<O>>(bytes, buffer).map(|_| ())
    }
}

impl<O: Offset> Verified<Text<O>> {
    /// The text, checked UTF-8 by the walk, so read here with no check.
    #[inline]
    pub fn as_str(&self) -> &str {
        // SAFETY: `Text`'s field is its first and only, at offset 0, of type `Slice<u8, O>`
        // (`#[repr(transparent)]`), which its deep check accepted as a slice.
        let bytes: &Verified<Slice<u8, O>> =
            unsafe { self.within(Verified::at(self) + size_of::<Slice<u8, O>>()) };
        let bytes = bytes.items().as_slice();
        // SAFETY: the invariant for a `Text`: its deep check accepted the bytes as UTF-8.
        unsafe { core::str::from_utf8_unchecked(bytes) }
    }
}
