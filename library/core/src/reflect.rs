//! Compile-time reflection over fields, variants and functions: blaze's `blaze_comptime`
//! extension (docs/v2/plan/design/comptime.md, phase 2).
//!
//! This module is blaze's, not the Rust project's (`library/UPSTREAM` lists it among blaze's
//! changes to the standard library). Every item is unstable under `blaze_comptime`, so a crate
//! that does not write `#![feature(blaze_comptime)]` can name none of them, and nothing here
//! changes what any other item of `core` does. rustc has no such feature (E0635) and no such
//! module, so a program that uses it is refused there, never built differently.
//!
//! What generic code ranges over:
//!
//! - [`Field<Base>`]: `F` is a field of `Base`. Its types are nightly's field representing
//!   types (`core::field::FieldRepresentingType<T, VARIANT, FIELD>`), so a reflected field's
//!   type is `<F as core::field::Field>::Type` and its offset `<F as core::field::Field>::OFFSET`.
//!   `for<F: Field<T>> F::Type: Hash` reads "the type of every field of `T` is `Hash`".
//! - [`Variant<Base>`]: `V` is a variant of the enum `Base`. Its types are
//!   [`VariantRepresentingType<Base, VARIANT>`]; the fields of the variant are the fields of its
//!   view, `V::View` ([`VariantOf<Base, VARIANT>`]): `for<V: Variant<T>, F: Field<V::View>>`.
//! - [`Marked<M>`] and [`Unmarked<M>`] narrow a field binder to the fields written (or not)
//!   `#[blaze::mark(M)]`; [`Fields::marked`] and [`Fields::unmarked`] narrow a `for const` loop
//!   the same way.
//!
//! Every trait here is answered by blaze's trait solver and none may be implemented by hand
//! (`#[rustc_deny_explicit_impl]`). Their constants are filled by blaze's const evaluator,
//! through the intrinsics at the end of this file, and are only ever evaluated at compile time:
//! each is a constant's initializer or an inline `const { .. }`, as `core::field::Field::OFFSET`
//! is, so no call of them reaches code generation.

use crate::any::TypeId;
use crate::marker::{DiscriminantKind, PhantomData};
use crate::{fmt, mem};

/// A struct or tuple whose fields reflection may read and write: one whose every field is
/// `pub`, or a struct written `#[blaze::reflect]`, and the view [`VariantOf<E, V>`] of an
/// enum's variant. Unions are not (no field can be read safely), nor are `repr(packed)` structs
/// (a borrow of a reflected field could be unaligned).
///
/// # Safety
///
/// Given a valid `Self`, each of its fields that [`fields_of::<Self>()`](fields_of) yields is a
/// valid value of the field's type at the field's offset. Only the trait solver implements it.
#[lang = "blaze_struct"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub unsafe trait Struct {
    /// The struct's identifier, as `#[derive(Debug)]` prints it (`Point` for `a::Point<u8>`);
    /// the variant's for a [`VariantOf`]; empty for a tuple.
    const NAME: &'static str = intrinsics::blaze_adt_name::<Self>();
}

/// An enum. Every enum is one: a variant's fields are as visible as the enum.
///
/// # Safety
///
/// `Self` is an enum, and each [`Variant<Self>`]'s view is a valid way to read a value of
/// `Self` whose discriminant is that variant's. Only the trait solver implements it.
#[lang = "blaze_enum"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub unsafe trait Enum: Sized {
    /// The enum's identifier, as `#[derive(Debug)]` would name it (`Shape` for `a::Shape<u8>`).
    const NAME: &'static str = intrinsics::blaze_adt_name::<Self>();
}

/// `Self` is a field of `Base`: what `for<F: Field<T>>` ranges over and the type of the
/// constant a `for const f in fields_of::<T>()` loop binds.
///
/// Implemented by the trait solver for `FieldRepresentingType<T, 0, i>` with `Base = T` (a
/// [`Struct`] or tuple, `i` below its field count) and for `FieldRepresentingType<T, v, i>`
/// with `Base = VariantOf<T, v>` (a field of variant `v` of the enum `T`). Nightly's
/// `<Self as core::field::Field>::Base` stays `T` in both: a field of an enum exists only in one
/// variant, which is why the container here is the variant's view.
///
/// # Safety
///
/// As `core::field::Field`'s: given a valid `Base`, a valid `Self::Type` is at `Self::OFFSET`.
#[lang = "blaze_field"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub const unsafe trait Field<Base: ?Sized>: crate::field::Field {
    /// The field's place in its container's declaration order.
    const INDEX: usize = intrinsics::blaze_field_index::<Self>();

    /// The field's identifier; `"0"`, `"1"`, .. for a tuple's or a tuple struct's.
    const NAME: &'static str = {
        let frt = crate::intrinsics::reflection::type_id::<Self>();
        crate::intrinsics::field_representing_type_name(frt)
    };

    /// The names of the field's `#[blaze::attr(name)]` attributes, in the order written.
    const ATTRS: &'static [&'static str] = intrinsics::blaze_field_attrs::<Self>();

    /// The field's identifier ([`Self::NAME`]).
    fn name(self) -> &'static str {
        Self::NAME
    }

    /// The field's place in declaration order ([`Self::INDEX`]).
    fn index(self) -> usize {
        Self::INDEX
    }

    /// Whether the field is written `#[blaze::attr(name)]`. For pruning a copy of a loop's body
    /// with `if const { f.has_attr("skip") }`; a string never narrows a bound (see [`Marked`]).
    fn has_attr(self, name: &str) -> bool {
        let mut at = 0;
        while at < Self::ATTRS.len() {
            if same_str(Self::ATTRS[at], name) {
                return true;
            }
            at += 1;
        }
        false
    }
}

/// `Self` is a field written `#[blaze::mark(M)]`. `M` is a type, so a mark is a resolved path:
/// a misspelt mark is an unresolved name, never a field quietly let through.
/// `for<F: Field<T> + Marked<Key>> ..` ranges over the fields of `T` marked `Key`.
#[lang = "blaze_marked"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub trait Marked<M: ?Sized>: crate::field::Field {}

/// `Self` is a field not written `#[blaze::mark(M)]`: `for<F: Field<T> + Unmarked<Skip>> ..`
/// ranges over the fields of `T` not marked `Skip`.
#[lang = "blaze_unmarked"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub trait Unmarked<M: ?Sized>: crate::field::Field {}

/// The variant `VARIANT` (its index in declaration order) of the enum `T`, as a type: what
/// [`Variant<T>`] is implemented for and the type of the constant a
/// `for const v in variants_of::<T>()` loop binds. Zero-sized; made only by the loop.
#[lang = "blaze_variant_representing_type"]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct VariantRepresentingType<T: ?Sized, const VARIANT: u32> {
    // Invariant over `T`, as `FieldRepresentingType` is.
    _phantom: PhantomData<fn(T) -> T>,
}

impl<T: ?Sized, const VARIANT: u32> Clone for VariantRepresentingType<T, VARIANT> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized, const VARIANT: u32> Copy for VariantRepresentingType<T, VARIANT> {}

impl<T: ?Sized, const VARIANT: u32> fmt::Debug for VariantRepresentingType<T, VARIANT> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_name = const { crate::any::type_name::<T>() };
        write!(f, "VariantRepresentingType<{type_name}, {VARIANT}>")
    }
}

/// `Self` is a variant of the enum `Base`: what `for<V: Variant<T>>` ranges over. Implemented
/// by the trait solver for `VariantRepresentingType<Base, v>`, `v` below `Base`'s variant count.
///
/// # Safety
///
/// `Self::View` is `VariantOf<Base, Self::INDEX>`, and `Self::DISCRIMINANT` is the
/// discriminant of that variant: a `Base` whose discriminant it is may be read as a `View`.
#[lang = "blaze_variant"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub const unsafe trait Variant<Base: Enum>: Copy + Send + Sync {
    /// `VariantOf<Base, INDEX>`: a `Base` known to be this variant, whose fields
    /// ([`fields_of::<V::View>()`](fields_of), `Field<V::View>`) are the variant's. The bound
    /// is what lets generic code call `fields_of::<V::View>()` for a `V` it knows only as a
    /// `Variant<T>`.
    #[lang = "blaze_variant_view"]
    type View: Struct;

    /// The variant's place in declaration order.
    const INDEX: usize = intrinsics::blaze_variant_index::<Self>();

    /// The variant's identifier.
    const NAME: &'static str = crate::intrinsics::reflection::type_id_variant_name(
        crate::intrinsics::reflection::type_id::<Base>(),
        Self::INDEX,
    );

    /// The discriminant of a `Base` that is this variant, as `discriminant_value` reads it.
    const DISCRIMINANT: <Base as DiscriminantKind>::Discriminant =
        intrinsics::blaze_variant_discriminant::<Self, Base>();

    /// The variant's identifier ([`Self::NAME`]).
    fn name(self) -> &'static str {
        Self::NAME
    }

    /// The variant's place in declaration order ([`Self::INDEX`]).
    fn index(self) -> usize {
        Self::INDEX
    }

    /// The variant's fields, for `for const g in v.fields() { .. }` inside a
    /// `for const v in variants_of::<T>()` loop, where `v`'s type has no name to write
    /// `fields_of::<V::View>()` with.
    fn fields(self) -> Fields<Self::View> {
        fields_of::<Self::View>()
    }

    /// `value` as this variant, when it is this variant.
    fn of(self, value: &Base) -> Option<&Self::View> {
        if same_bytes(&crate::intrinsics::discriminant_value(value), &Self::DISCRIMINANT) {
            // SAFETY: `View` is `VariantOf<Base, INDEX>`, `repr(transparent)` over `Base`, and
            // `value`'s discriminant is the variant's (the trait's contract).
            Some(unsafe { &*(value as *const Base as *const Self::View) })
        } else {
            None
        }
    }

    /// `value` as this variant, when it is this variant.
    fn of_mut(self, value: &mut Base) -> Option<&mut Self::View> {
        if same_bytes(&crate::intrinsics::discriminant_value(value), &Self::DISCRIMINANT) {
            // SAFETY: as in `of`; the borrow is `value`'s, so it is unique.
            Some(unsafe { &mut *(value as *mut Base as *mut Self::View) })
        } else {
            None
        }
    }
}

/// A `T` known to be its variant `VARIANT`: the same address and layout as `T`
/// (`repr(transparent)`), made only by [`Variant::of`] and [`Variant::of_mut`]. Its fields, to
/// reflection, are the variant's: `x.[g]` on it is a downcast to the variant, then the field.
#[lang = "blaze_variant_of"]
#[repr(transparent)]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct VariantOf<T: ?Sized, const VARIANT: u32> {
    // Read only through reflection's projections (`x.[g]`), never by name.
    #[allow(dead_code)]
    value: T,
}

/// What `for const f in ..` iterates over the fields of `T`: zero-sized; the loop reads only its
/// type. `P` is the filter, [`OnlyMarked`] and [`OnlyUnmarked`] links ending in `()` (none).
#[lang = "blaze_fields"]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct Fields<T: ?Sized, P = ()> {
    _phantom: PhantomData<(fn(T) -> T, fn(P) -> P)>,
}

impl<T: ?Sized, P> Clone for Fields<T, P> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized, P> Copy for Fields<T, P> {}

impl<T: ?Sized, P> Fields<T, P> {
    /// Only the fields written `#[blaze::mark(M)]`: the loop's field then also assumes
    /// `F: Marked<M>`, so a bound `for<F: Field<T> + Marked<M>> ..` holds for it.
    #[unstable(feature = "blaze_comptime", issue = "none")]
    pub const fn marked<M: ?Sized>(self) -> Fields<T, OnlyMarked<M, P>> {
        Fields { _phantom: PhantomData }
    }

    /// Only the fields not written `#[blaze::mark(M)]`: the loop's field then also assumes
    /// `F: Unmarked<M>`.
    #[unstable(feature = "blaze_comptime", issue = "none")]
    pub const fn unmarked<M: ?Sized>(self) -> Fields<T, OnlyUnmarked<M, P>> {
        Fields { _phantom: PhantomData }
    }
}

/// A [`Fields`] filter: the fields [`Marked<M>`], and those `Rest` keeps. Never made.
#[lang = "blaze_only_marked"]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct OnlyMarked<M: ?Sized, Rest = ()> {
    _phantom: PhantomData<(fn(M) -> M, fn(Rest) -> Rest)>,
}

/// A [`Fields`] filter: the fields [`Unmarked<M>`], and those `Rest` keeps. Never made.
#[lang = "blaze_only_unmarked"]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct OnlyUnmarked<M: ?Sized, Rest = ()> {
    _phantom: PhantomData<(fn(M) -> M, fn(Rest) -> Rest)>,
}

/// What `for const v in ..` iterates over the variants of the enum `T`: zero-sized; the loop
/// reads only its type.
#[lang = "blaze_variants"]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct Variants<T: ?Sized> {
    _phantom: PhantomData<fn(T) -> T>,
}

impl<T: ?Sized> Clone for Variants<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for Variants<T> {}

/// The fields of `T`, in declaration order, for `for const f in fields_of::<T>() { .. }`. For
/// an enum's variant, `T` is its view: `fields_of::<V::View>()`.
#[unstable(feature = "blaze_comptime", issue = "none")]
pub const fn fields_of<T: ?Sized + Struct>() -> Fields<T> {
    Fields { _phantom: PhantomData }
}

/// The variants of the enum `T`, in declaration order, for
/// `for const v in variants_of::<T>() { .. }`.
#[unstable(feature = "blaze_comptime", issue = "none")]
pub const fn variants_of<T: Enum>() -> Variants<T> {
    Variants { _phantom: PhantomData }
}

/// A function item's or a closure's type: the types whose code is known at compile time.
/// Never a function pointer's. Implemented by the trait solver.
///
/// # Safety
///
/// `Self` is a fn item type (with its generic arguments) or a closure type. Only the trait
/// solver implements it.
#[lang = "blaze_fn_item"]
#[rustc_deny_explicit_impl]
#[rustc_dyn_incompatible_trait]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub unsafe trait FnItem {}

/// Const facts about a function item or a closure, from [`fn_info`].
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
#[unstable(feature = "blaze_comptime", issue = "none")]
pub struct FnInfo {
    /// The function's identifier; a closure's is its enclosing function's.
    pub name: &'static str,
    /// The function's path, with its generic arguments, as `type_name` prints its item type.
    pub path: &'static str,
    /// The behaviour identity of the instance (`hir_ty_identity::fn_code_id`): what running it
    /// can depend on, over everything it can call, so it changes when the function or any code
    /// it can run changes, and not for a comment or a move. Code whose constants ask for the id
    /// of code that runs them is refused, naming both, never given a guessed id.
    pub code_id: [u8; 32],
    /// The `TypeId`s of the inputs, in order.
    pub inputs: &'static [TypeId],
    /// The `TypeId` of the output.
    pub output: TypeId,
    /// Whether it is a `const fn` (a `const` closure).
    pub is_const: bool,
    /// Whether it is an `unsafe fn`.
    pub is_unsafe: bool,
}

impl FnInfo {
    /// Const facts about the function item or closure type `F`: a constant in every context,
    /// runtime or const, as it is evaluated in a `const { .. }` for each instance of `F`.
    #[unstable(feature = "blaze_comptime", issue = "none")]
    pub const fn of<F: FnItem>() -> FnInfo {
        const { intrinsics::blaze_fn_info::<F>() }
    }
}

/// Const facts about the function item or closure `f` ([`FnInfo::of::<F>()`](FnInfo::of)).
/// Taken by reference so that every closure qualifies, not only those that are `Copy`:
/// `fn_info(&step).code_id`.
#[unstable(feature = "blaze_comptime", issue = "none")]
pub const fn fn_info<F: FnItem>(f: &F) -> FnInfo {
    let _ = f;
    FnInfo::of::<F>()
}

/// Whether `a` and `b` are the same bytes. For discriminants, which are integers, so every byte
/// is initialized; `PartialEq` of a discriminant type is not usable in a `const` trait's body.
const fn same_bytes<D>(a: &D, b: &D) -> bool {
    let a = a as *const D as *const u8;
    let b = b as *const D as *const u8;
    let mut at = 0;
    while at < mem::size_of::<D>() {
        // SAFETY: both point to a `D` and `at` is below its size; a discriminant has no padding.
        if unsafe { *a.add(at) != *b.add(at) } {
            return false;
        }
        at += 1;
    }
    true
}

/// `a == b`, in a `const` body.
const fn same_str(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut at = 0;
    while at < a.len() {
        if a[at] != b[at] {
            return false;
        }
        at += 1;
    }
    true
}

/// What blaze's const evaluator answers for reflection. Each is `#[rustc_comptime]` and called
/// only from a constant's initializer or an inline `const { .. }`, so none reaches code
/// generation. rustc has none of them.
mod intrinsics {
    /// The identifier of the struct, enum or variant view `T` ([`super::Struct::NAME`],
    /// [`super::Enum::NAME`]): empty for a tuple, the variant's for a `VariantOf`.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_adt_name<T: ?Sized>() -> &'static str;

    /// `FIELD` of the field representing type `F`: the field's place in declaration order.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_field_index<F: crate::field::Field>() -> usize;

    /// The names of the `#[blaze::attr(name)]` attributes of the field `F` stands for.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_field_attrs<F: crate::field::Field>() -> &'static [&'static str];

    /// `VARIANT` of the variant representing type `V`.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_variant_index<V>() -> usize;

    /// The discriminant of the variant `V` (a `VariantRepresentingType<E, v>`) of `E`.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_variant_discriminant<V, E>()
    -> <E as crate::marker::DiscriminantKind>::Discriminant;

    /// [`super::FnInfo`] of the fn item or closure type `F`.
    #[rustc_intrinsic]
    #[rustc_comptime]
    pub(super) fn blaze_fn_info<F>() -> super::FnInfo;
}
