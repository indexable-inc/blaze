//! A record declared in safe code: [`Declared`], which [`crate::record!`] implements, and the
//! one `unsafe impl` of [`Record`] over it, which lives here.
//!
//! # Why a crate that declares records holds no `unsafe`
//!
//! [`Record`] is an `unsafe trait`: [`crate::open`] and [`crate::Rel::get`] hand out `&T` over
//! bytes they have only checked with `T::check`, so its implementor promises that `T` has no
//! padding and that every bit pattern `check` accepts is a valid `T`. A struct's promise
//! follows from three facts: (1) its fields are exactly the types its `check` checks, each at
//! the offset where it checks it; (2) the fields fill the struct, so there is no padding; (3)
//! each field's own type is a `Record`, whose promise covers its bytes. A struct whose fields
//! are all valid is a valid struct, as Rust's validity of a struct is the validity of its
//! fields, and with no padding every byte belongs to some field.
//!
//! The old `record!` wrote the `unsafe impl` into the declaring crate, so that crate held
//! `unsafe` and could not `forbid(unsafe_code)`. Now the declaring crate implements
//! [`Declared`], a safe trait, and names its fields' types as a list ([`Fields`]); the one
//! `unsafe impl<T: Declared> Record for T` is below. A safe trait can be implemented by hand,
//! so the impl can't trust what a `Declared` impl says: it checks it. [`Layout::VERDICT`] reads
//! the struct itself by compile-time reflection (`core::mem::type_info`): that it is a struct,
//! how many fields it has, each field's type and offset (fact 1, now the compiler's word, not
//! the declaration's), and that the declared types' sizes sum to the struct's size (fact 2: the
//! fields of a struct never overlap, so if their sizes sum to its size they fill it). Fact 3
//! is the `F: Record` bound of each list entry. Every place in this crate that turns bytes into
//! a `&T` or a `T` into bytes (`raw.rs`) first names [`Record::VERIFIED`], which for a
//! declared `T` is the verdict's assertion: a declaration that lies does not compile into any
//! program that views it. `record!` also asserts the verdict where it declares the struct, so
//! the mistake is reported there, at `cargo check` time, with the struct's name.
//!
//! [`Layout`] and [`FieldList`] are sealed (their supertrait is unnameable outside this crate)
//! and `unsafe`, so the only layouts are the two here: a struct's [`Fields`], and a fieldless
//! one-byte enum's [`ByteEnum`], checked by round trip (a byte is accepted only when it is the
//! byte of the value [`FromByte::from_byte`] made of it, so it is the byte of a valid value
//! whatever `from_byte` does). What a declaration still says unchecked is its schema hash,
//! which is identity, not soundness: a wrong one refuses buffers, it never misreads one.

use core::any::TypeId;
use core::mem::size_of;
use core::mem::type_info::{self, Type, TypeKind};

use crate::{
    FormatError, Record, SchemaHash, WordTables, check_field, check_field_shallow, field_columns,
    raw,
};

/// A record declared in safe code: its schema, and its layout, which the format checks against
/// the type itself (the module's comment says how). [`crate::record!`] implements it; so may a
/// hand, for a shape the macro doesn't spell.
pub trait Declared: Copy + 'static {
    /// The schema hash, as [`Record::SCHEMA`].
    const SCHEMA: SchemaHash;
    /// What the bytes are: [`Fields`] of a list of the field types in order, or [`ByteEnum`].
    type Layout: Layout<Self>;
}

/// Asserts at compile time that `T`'s declaration is its type's: what [`crate::record!`] writes
/// beside the struct, so a wrong declaration is refused there and at `cargo check`.
pub const fn assert_declared<T: Declared>() {
    <T::Layout as Layout<T>>::VERDICT.assert();
}

mod sealed {
    pub trait Sealed<S> {}
    pub trait List<S> {}
}

/// Whether a declaration is its type's, found by reflection: one answer per way it can fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Sound,
    /// [`Fields`] of a type that is not a struct.
    NotAStruct,
    /// A list of more or fewer types than the struct has fields.
    FieldCount,
    /// A list whose type at some place is not the struct's field's there.
    FieldType,
    /// Fields whose sizes don't sum to the struct's: there is padding.
    Padding,
    /// [`ByteEnum`] of a type that is not an enum of one byte whose variants hold nothing.
    NotAByteEnum,
}

impl Verdict {
    /// Panics, at compile time where it is a constant, unless the declaration is sound.
    pub const fn assert(self) {
        match self {
            Verdict::Sound => {}
            Verdict::NotAStruct => panic!("a record declared by its fields that is not a struct"),
            Verdict::FieldCount => {
                panic!("a record declared with more or fewer fields than its struct has")
            }
            Verdict::FieldType => {
                panic!("a record declared with a field type that is not its struct's field's")
            }
            Verdict::Padding => panic!("a record with padding: order its fields by alignment"),
            Verdict::NotAByteEnum => {
                panic!("a byte enum that is not a one-byte enum whose variants hold no fields")
            }
        }
    }
}

/// How a [`Declared`] record's bytes are checked and walked. Implemented only here.
///
/// # Safety
///
/// When [`Layout::VERDICT`] is [`Verdict::Sound`], every `size_of::<S>()` bytes that `check`
/// (or `check_shallow`, the references in them unfollowed) accepts are a valid `S`, every
/// `S` has no padding, and `PLAIN`, `SHALLOW_PLAIN` and `COLUMNS` are true only as
/// [`Record`]'s contract allows.
pub unsafe trait Layout<S>: sealed::Sealed<S> {
    const PLAIN: bool;
    const SHALLOW_PLAIN: bool;
    const COLUMNS: bool;
    const VERDICT: Verdict;
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
    fn columns<O: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut O);
}

/// A struct's layout: its fields' types, in order, as a list `(F0, (F1, (.. , ())))`, which
/// [`crate::record!`] spells. Offsets are not declared: they are read from the struct.
pub struct Fields<L>(L);

/// A list of field types of the struct `S`, `(F, rest)` or `()`. Implemented only here.
///
/// # Safety
///
/// `TYPES` is true only when the list's types are `S`'s last `LEN` fields' types, in order,
/// and `check` and `check_shallow` check each at that field's offset.
pub unsafe trait FieldList<S>: sealed::List<S> {
    const LEN: usize;
    const SIZE: usize;
    const PLAIN: bool;
    const SHALLOW_PLAIN: bool;
    const COLUMNS: bool;
    const TYPES: bool;
    fn check(record: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
    fn check_shallow(record: &[u8], buffer: &[u8]) -> Result<(), FormatError>;
    fn columns<O: WordTables + ?Sized>(record: &[u8], buffer: &[u8], out: &mut O);
}

impl<S> sealed::List<S> for () {}

// SAFETY: no types and nothing checked: the last 0 fields of any struct.
unsafe impl<S> FieldList<S> for () {
    const LEN: usize = 0;
    const SIZE: usize = 0;
    const PLAIN: bool = true;
    const SHALLOW_PLAIN: bool = true;
    const COLUMNS: bool = false;
    const TYPES: bool = true;
    fn check(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }
    fn check_shallow(_: &[u8], _: &[u8]) -> Result<(), FormatError> {
        Ok(())
    }
    fn columns<O: WordTables + ?Sized>(_: &[u8], _: &[u8], _: &mut O) {}
}

impl<S, F, R: FieldList<S>> sealed::List<S> for (F, R) {}

/// Which of `S`'s fields a list entry is: the one `rest` fields before the end, as reflection
/// numbers them (source order, which is the layout's for `#[repr(C)]`).
const fn index_of<S: 'static>(rest: usize) -> Option<usize> {
    let count = const { TypeId::of::<S>().fields(0) };
    match rest < count {
        true => Some(count - 1 - rest),
        false => None,
    }
}

impl<S: 'static, F: Record, R: FieldList<S>> FieldOf<S> for (F, R) {
    const OFFSET: usize = match index_of::<S>(R::LEN) {
        Some(index) => TypeId::of::<S>().field(0, index).offset(),
        None => 0,
    };
}

/// Where a list entry's field is in `S`, from reflection.
trait FieldOf<S> {
    const OFFSET: usize;
}

// SAFETY: `TYPES` holds only when reflection says `S`'s field `index_of(R::LEN)` is an `F`
// and `R`'s `TYPES` holds for the fields after it; `OFFSET` is that field's offset, read from
// the struct, and `check` checks an `F` there by `F::check` (sound by `F: Record`), then the
// rest. `PLAIN` and `SHALLOW_PLAIN` are each field's, so they hold only when every field's
// bytes are valid unchecked.
unsafe impl<S: 'static, F: Record, R: FieldList<S>> FieldList<S> for (F, R) {
    const LEN: usize = R::LEN + 1;
    const SIZE: usize = size_of::<F>() + R::SIZE;
    const PLAIN: bool = F::PLAIN && R::PLAIN;
    const SHALLOW_PLAIN: bool = F::SHALLOW_PLAIN && R::SHALLOW_PLAIN;
    const COLUMNS: bool = F::COLUMNS || R::COLUMNS;
    const TYPES: bool = match index_of::<S>(R::LEN) {
        Some(index) => {
            TypeId::of::<S>().field(0, index).type_id() == type_info::of::<F>() && R::TYPES
        }
        None => false,
    };
    #[inline]
    fn check(record: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        check_field::<F>(record, <Self as FieldOf<S>>::OFFSET, buffer)?;
        R::check(record, buffer)
    }
    #[inline]
    fn check_shallow(record: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        check_field_shallow::<F>(record, <Self as FieldOf<S>>::OFFSET, buffer)?;
        R::check_shallow(record, buffer)
    }
    #[inline]
    fn columns<O: WordTables + ?Sized>(record: &[u8], buffer: &[u8], out: &mut O) {
        field_columns::<F, O>(record, <Self as FieldOf<S>>::OFFSET, buffer, out);
        R::columns(record, buffer, out);
    }
}

impl<S, L: FieldList<S>> sealed::Sealed<S> for Fields<L> {}

// SAFETY: sound when the verdict is: `S` is a struct whose fields are the list's types in
// order (count and each type from reflection), each checked at its own offset by its own
// `Record` check, and whose fields' sizes sum to its size, so no byte is padding.
unsafe impl<S: 'static, L: FieldList<S>> Layout<S> for Fields<L> {
    const PLAIN: bool = L::PLAIN;
    const SHALLOW_PLAIN: bool = L::SHALLOW_PLAIN;
    const COLUMNS: bool = L::COLUMNS;
    const VERDICT: Verdict = match Type::of::<S>().kind {
        TypeKind::Struct if TypeId::of::<S>().fields(0) != L::LEN => Verdict::FieldCount,
        TypeKind::Struct if !L::TYPES => Verdict::FieldType,
        TypeKind::Struct if L::SIZE != size_of::<S>() => Verdict::Padding,
        TypeKind::Struct => Verdict::Sound,
        _ => Verdict::NotAStruct,
    };
    #[inline]
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        L::check(bytes, buffer)
    }
    #[inline]
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        L::check_shallow(bytes, buffer)
    }
    #[inline]
    fn columns<O: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut O) {
        L::columns(bytes, buffer, out);
    }
}

/// A fieldless enum of one byte, as a record: [`ByteEnum`]'s values, made from their byte.
pub trait FromByte: Copy + 'static {
    /// What [`FormatError::Invalid`] says of a byte that is no value.
    const WHAT: &'static str;
    /// The value whose byte is `byte`, `None` when there is none.
    fn from_byte(byte: u8) -> Option<Self>;
}

/// The layout of a fieldless enum one byte long: a byte is accepted when it is the byte of the
/// value [`FromByte::from_byte`] makes of it.
pub struct ByteEnum;

impl<S: FromByte> sealed::Sealed<S> for ByteEnum {}

/// Whether every variant of `S`, an enum, holds no field: reflection's calls are made only at
/// compile time, so this is a constant (`FIELDLESS`), not a `const fn`.
struct Fieldless<S>(S);

impl<S: 'static> Fieldless<S> {
    const FIELDLESS: bool = {
        let id = TypeId::of::<S>();
        let (variants, mut variant, mut fieldless) = (id.variants(), 0, true);
        while variant < variants {
            fieldless &= id.fields(variant) == 0;
            variant += 1;
        }
        fieldless
    };
}

// SAFETY: sound when the verdict is: `S` is an enum one byte long whose variants hold nothing,
// so its byte is its discriminant, and `check` accepts a byte only when it equals the byte of
// a valid `S` (`from_byte`'s, read by `raw::enum_byte`). Nothing is plain, no table is in it.
unsafe impl<S: FromByte> Layout<S> for ByteEnum {
    const PLAIN: bool = false;
    const SHALLOW_PLAIN: bool = false;
    const COLUMNS: bool = false;
    const VERDICT: Verdict = match Type::of::<S>().kind {
        TypeKind::Enum if size_of::<S>() == 1 && Fieldless::<S>::FIELDLESS => Verdict::Sound,
        _ => Verdict::NotAByteEnum,
    };
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        match bytes {
            [byte] if S::from_byte(*byte).is_some_and(|value| raw::enum_byte(&value) == *byte) => {
                Ok(())
            }
            _ => Err(FormatError::Invalid {
                at: (bytes.as_ptr() as usize).wrapping_sub(buffer.as_ptr() as usize) as u64,
                what: S::WHAT,
            }),
        }
    }
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        <ByteEnum as Layout<S>>::check(bytes, buffer)
    }
    fn columns<O: WordTables + ?Sized>(_: &[u8], _: &[u8], _: &mut O) {}
}

// SAFETY: the module's comment: `VERIFIED` is the layout's verdict asserted, which every view
// and every read of a record's bytes in this crate (`raw.rs`) names first, so no `T` is viewed
// unless its declaration is its type's; then `Layout`'s contract is `Record`'s. `check` and
// `check_shallow` return early exactly where `PLAIN` and `SHALLOW_PLAIN` say any bytes are a
// value, as the old `record!` did.
unsafe impl<T: Declared> Record for T {
    const SCHEMA: SchemaHash = <T as Declared>::SCHEMA;
    const PLAIN: bool = <T::Layout as Layout<T>>::PLAIN;
    const SHALLOW_PLAIN: bool = <T::Layout as Layout<T>>::SHALLOW_PLAIN;
    const COLUMNS: bool = <T::Layout as Layout<T>>::COLUMNS;
    const VERIFIED: () = <T::Layout as Layout<T>>::VERDICT.assert();

    #[inline]
    fn check(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        match <T as Record>::PLAIN {
            true => Ok(()),
            false => <T::Layout as Layout<T>>::check(bytes, buffer),
        }
    }

    #[inline]
    fn check_shallow(bytes: &[u8], buffer: &[u8]) -> Result<(), FormatError> {
        match <T as Record>::SHALLOW_PLAIN {
            true => Ok(()),
            false => <T::Layout as Layout<T>>::check_shallow(bytes, buffer),
        }
    }

    #[inline]
    fn columns<O: WordTables + ?Sized>(bytes: &[u8], buffer: &[u8], out: &mut O) {
        <T::Layout as Layout<T>>::columns(bytes, buffer, out);
    }
}

/// The list type of field types `$ty, ..` for [`Fields`]: `($ty0, ($ty1, (.., ())))`.
#[doc(hidden)]
#[macro_export]
macro_rules! __record_fields {
    () => { () };
    ($head:ty $(, $rest:ty)*) => { ($head, $crate::__record_fields!($($rest),*)) };
}

/// Declares a `#[repr(C)]` struct of [`Record`] fields and makes it a record by [`Declared`],
/// in safe code (the module [`crate::declared`] says why that is sound): its schema from its
/// name, its version (the number after the name), and each field's name, offset and schema;
/// its check field by field. Fails to compile if the struct has padding: order the fields by
/// alignment, or spell the padding out as a `[u8; N]` field. The struct must derive `Clone`
/// and `Copy`. The expansion names `::core` and this crate only, so a `#![no_std]` crate that
/// forbids `unsafe_code` declares records with it.
///
/// ```ignore
/// blaze_format::record! {
///     #[derive(Clone, Copy, Debug)]
///     pub struct Node: 1 {
///         pub kind: u32,
///         pub children: Slice<u32>,
///     }
/// }
/// ```
#[macro_export]
macro_rules! record {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident: $version:literal {
            $( $(#[$field_meta:meta])* $field_vis:vis $field:ident: $ty:ty ),* $(,)?
        }
    ) => {
        $(#[$meta])*
        #[repr(C)]
        $vis struct $name {
            $( $(#[$field_meta])* $field_vis $field: $ty, )*
        }

        const _: () = ::core::assert!(
            ::core::mem::size_of::<$name>() == 0 $( + ::core::mem::size_of::<$ty>() )*,
            ::core::concat!(::core::stringify!($name), " has padding: order its fields by alignment"),
        );

        impl $crate::declared::Declared for $name {
            const SCHEMA: $crate::SchemaHash = $crate::schema_of(&[
                $crate::Part::Str(::core::stringify!($name)),
                $crate::Part::U64($version),
                $(
                    $crate::Part::Str(::core::stringify!($field)),
                    $crate::Part::U64(::core::mem::offset_of!($name, $field) as u64),
                    $crate::Part::Schema(<$ty as $crate::Record>::SCHEMA),
                )*
            ]);
            type Layout = $crate::declared::Fields<$crate::__record_fields!($($ty),*)>;
        }

        const _: () = $crate::declared::assert_declared::<$name>();
    };
}
