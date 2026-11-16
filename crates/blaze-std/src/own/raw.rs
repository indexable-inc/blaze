//! Reflection as the walk reads it ([`reach`](super::reach)): `core::mem::type_info` and
//! `core::reflect`, each fact one small function over a `TypeId`. Every fact but [`name`] is
//! the pinned nightly's own `type_info`, so the walk runs under rustc too; `core::reflect` is
//! blaze's, and [`name`] has a plain form under `cfg(not(blaze))`. The front end's tests give
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
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn id<T: ?Sized>() -> TypeId {
    type_info::of::<T>()
}

/// Whether two ids are of one type.
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn same(a: TypeId, b: TypeId) -> bool {
    a == b
}

/// What a type with no `Owns` impl is.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn shape(id: TypeId) -> Shape {
    // A tuple is never asked of `info`: the pinned nightly (16d1ffac17ad) panics there on every
    // tuple, a rustc bug. Its `write_type_info` (rustc_const_eval, const_eval/type_info.rs)
    // writes a `Tuple` payload into `TypeKind::Tuple`, which the same commit's core declares
    // with no field, so it indexes field 0 of none (rustc_abi/src/lib.rs:1763). So a tuple is
    // known by intrinsics that answer for every type: `()` by its id, and any other by its
    // first field, named `0`. A tuple struct and an enum whose first variant is tuple-like
    // match too, and are `Parts` either way; a union's fields are always named.
    if is_tuple_like(id) {
        return Shape::Parts;
    }
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

/// Whether the type is `()`, or its first variant's first field is named `0` (a tuple, a tuple
/// struct, an enum whose first variant is tuple-like). Asked only of `type_id_fields` and the
/// field's name, which answer for every type (`fields` is 0 for one with none), never of
/// `info` ([`shape`] says why).
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
fn is_tuple_like(id: TypeId) -> bool {
    if same(id, self::id::<()>()) {
        return true;
    }
    // An enum with no variants has no variant 0 to ask of.
    if variants(id) == 0 || fields(id, 0) == 0 {
        return false;
    }
    let name = field_name(id, 0, 0).as_bytes();
    name.len() == 1 && name[0] == b'0'
}

/// How many variants: an enum's, one for anything else.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn variants(id: TypeId) -> usize {
    id.variants()
}

/// How many fields the variant has.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn fields(id: TypeId, variant: usize) -> usize {
    id.fields(variant)
}

/// The type of a field.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn field(id: TypeId, variant: usize, field: usize) -> TypeId {
    id.field(variant, field).type_id()
}

/// The name of a field: a tuple's is its index.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn field_name(id: TypeId, variant: usize, field: usize) -> &'static str {
    id.field(variant, field).name()
}

/// The name of an enum's variant.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn variant_name(id: TypeId, variant: usize) -> &'static str {
    id.variant(variant).name()
}

/// What an array or slice holds, what a reference or raw pointer points at.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn inner(id: TypeId) -> Option<TypeId> {
    match id.element_ty() {
        Some(element) => Some(element),
        None => id.points_to(),
    }
}

/// How many generic arguments the type has.
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn args(id: TypeId) -> usize {
    id.generics().len()
}

/// The generic argument at `at`, when it is a type.
#[rustc_comptime]
#[blaze::noalloc]
#[blaze::nonblocking]
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
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn implements(id: TypeId, marker: TypeId) -> bool {
    if core::intrinsics::reflection::type_id_vtable(id, marker).is_some() {
        return true;
    }
    // A function pointer is asked as `fn()`: under the pinned nightly the intrinsic found no
    // impl of `bstd::flat::Address` for `fn(u32) -> u32` through its blanket
    // `impl<F: FnPtr> Address for F` (`tests::flat` saw the walk clear it), so a trait that
    // holds of every function pointer states it for `fn()` too, and is found there. Right for a
    // trait held of all function pointers or of none, which is the only kind the walk asks.
    id.function_ptr().is_some() && core::intrinsics::reflection::type_id_vtable(self::id::<fn()>(), marker).is_some()
}

/// The type as a diagnostic names it (`Shared<Parent>`).
#[cfg(blaze)]
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn name(id: TypeId) -> &'static str {
    core::reflect::name_of(id)
}

/// The type as a diagnostic names it, under rustc: its reflection names no type from a
/// `TypeId` (`core::reflect::name_of` is blaze's), so a refusal under rustc says where the walk
/// went by field and variant names only, and this stands for each type. The walk decides the
/// same; only its message is poorer (std.md, "Two builders").
#[cfg(not(blaze))]
#[rustc_comptime]
#[blaze::nopanic]
#[blaze::noalloc]
#[blaze::nonblocking]
pub fn name(id: TypeId) -> &'static str {
    let _ = id;
    "a type rustc's reflection does not name"
}

/// The first `len` bytes of `bytes` as text.
#[blaze::pure]
#[blaze::noalloc]
#[blaze::nonblocking]
pub const fn text(bytes: &[u8], len: usize) -> &str {
    // SAFETY: `len` is at most `bytes.len()` (`Reason` counts what it wrote into its own
    // array), and the bytes are whole `&str`s written one after another (`Reason::push`), so
    // they are UTF-8.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(bytes.as_ptr(), len)) }
}
