//! Reflection as the walk reads it ([`reach`](super::reach)): `core::mem::type_info` and
//! `core::reflect`, each fact one small function over a `TypeId`. The front end's tests give
//! the walk the same functions over the intrinsics alone, so this file is the only part of the
//! walk they do not evaluate.
//!
//! What reads a fact of a type is `#[rustc_comptime]`, as the function of `core` it calls is:
//! the nightly lets only a constant's value, a `const { .. }` block or another such function
//! call one, and no code is compiled for it. So the walk over these is `#[rustc_comptime]` too.

pub use core::any::TypeId;
use core::mem::type_info::{self, Generic, TypeKind};

use super::reach::Shape;

/// The id of `T`, which need not outlive `'static`: an instance is its own id.
#[blaze::pure]
pub const fn id<T: ?Sized>() -> TypeId {
    type_info::of::<T>()
}

/// Whether two ids are of one type.
#[blaze::nopanic]
pub const fn same(a: TypeId, b: TypeId) -> bool {
    a == b
}

/// What a type with no `Owns` impl is.
#[rustc_comptime]
#[blaze::nopanic]
pub fn shape(id: TypeId) -> Shape {
    match id.info().kind {
        TypeKind::Bool
        | TypeKind::Char
        | TypeKind::Int
        | TypeKind::Float
        | TypeKind::Str(_)
        | TypeKind::FnPtr => Shape::Leaf,
        TypeKind::Tuple | TypeKind::Struct | TypeKind::Enum => Shape::Parts,
        TypeKind::Array | TypeKind::Slice => Shape::Elements,
        TypeKind::Reference => Shape::Borrow,
        TypeKind::Pointer => Shape::Pointer,
        TypeKind::DynTrait(_) => Shape::Dyn,
        // A union, a closure, a foreign type, `!`, and whatever reflection adds later.
        _ => Shape::Unseen,
    }
}

/// How many variants: an enum's, one for anything else.
#[rustc_comptime]
#[blaze::nopanic]
pub fn variants(id: TypeId) -> usize {
    id.variants()
}

/// How many fields the variant has.
#[rustc_comptime]
#[blaze::nopanic]
pub fn fields(id: TypeId, variant: usize) -> usize {
    id.fields(variant)
}

/// The type of a field.
#[rustc_comptime]
#[blaze::nopanic]
pub fn field(id: TypeId, variant: usize, field: usize) -> TypeId {
    id.field(variant, field).type_id()
}

/// The name of a field: a tuple's is its index.
#[rustc_comptime]
#[blaze::nopanic]
pub fn field_name(id: TypeId, variant: usize, field: usize) -> &'static str {
    id.field(variant, field).name()
}

/// The name of an enum's variant.
#[rustc_comptime]
#[blaze::nopanic]
pub fn variant_name(id: TypeId, variant: usize) -> &'static str {
    id.variant(variant).name()
}

/// What an array or slice holds, what a reference or raw pointer points at.
#[rustc_comptime]
#[blaze::nopanic]
pub fn inner(id: TypeId) -> Option<TypeId> {
    match id.element_ty() {
        Some(element) => Some(element),
        None => id.points_to(),
    }
}

/// How many generic arguments the type has.
#[rustc_comptime]
#[blaze::nopanic]
pub fn args(id: TypeId) -> usize {
    id.generics().len()
}

/// The generic argument at `at`, when it is a type.
#[rustc_comptime]
#[blaze::noalloc]
pub fn arg(id: TypeId, at: usize) -> Option<TypeId> {
    match &id.generics()[at] {
        Generic::Type(it) => Some(it.ty),
        _ => None,
    }
}

/// Whether the type implements the trait whose `dyn` type has the id `marker`. Asked of the
/// intrinsic, not `TypeId::trait_info_of_trait_type_id`, which answers `None` for every
/// unsized type: a `dyn Job` is asked whether its trait vouches for it.
#[rustc_comptime]
#[blaze::nopanic]
pub fn implements(id: TypeId, marker: TypeId) -> bool {
    core::intrinsics::reflection::type_id_vtable(id, marker).is_some()
}

/// The type as a diagnostic names it (`Shared<Parent>`).
#[rustc_comptime]
#[blaze::nopanic]
pub fn name(id: TypeId) -> &'static str {
    core::reflect::name_of(id)
}

/// The first `len` bytes of `bytes` as text.
#[blaze::pure]
pub const fn text(bytes: &[u8], len: usize) -> &str {
    // SAFETY: `len` is at most `bytes.len()` (`Reason` counts what it wrote into its own
    // array), and the bytes are whole `&str`s written one after another (`Reason::push`), so
    // they are UTF-8.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(bytes.as_ptr(), len)) }
}
