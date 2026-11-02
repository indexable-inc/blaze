//! `hot_clone`: a call of `Clone::clone` whose `Self` is not copy-like.
//!
//! Copy-like: `Copy`; a reference, raw pointer, function, primitive or `!`; or a tuple, array or
//! ADT whose `Clone` is built in or made by `#[derive(Clone)]` (hir-def's built-in derive
//! impls, which have no body) and every field of which is copy-like. Anything else costs more
//! than a copy, and the finding says why: it allocates (a hand-written `Clone` that reaches the
//! allocator), bumps a reference count (`Arc`, `Rc` and their `Weak`s), runs a hand-written
//! `Clone`, clones a type not known to be `Copy` (a type parameter, a projection), or clones
//! what a closure captures.

use hir::HirDisplay as _;
use hir_def::{AdtId, HasModule as _, builtin_derive::BuiltinDeriveImplTrait};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{GenericArg, GenericArgs},
};

use super::{Known, alloc};

/// How deep derived `Clone`s nest before the walk stops: past it the type is reported, as no
/// type written by hand nests so many derived structs without a `Copy` one.
const DEEPEST: u8 = 16;

/// Why a clone costs more than a copy.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Why {
    Allocates,
    Counts,
    HandWritten,
    NotCopy,
    Captures,
    Deep,
}

impl Why {
    fn says(self) -> &'static str {
        match self {
            Why::Allocates => "allocates",
            Why::Counts => "bumps a reference count",
            Why::HandWritten => "runs a hand-written `Clone`",
            Why::NotCopy => "clones a type not known to be `Copy`",
            Why::Captures => "clones what a closure captures",
            Why::Deep => "nests derived `Clone`s deeper than blaze looks",
        }
    }
}

/// What makes a type not copy-like: why, and the type in it that does.
pub(super) struct Costly<'db> {
    pub(super) why: Why,
    pub(super) ty: hir::Type<'db>,
}

/// Whether the call `callee` (as written) is `Clone::clone`, the trait's or an impl's.
pub(super) fn is_clone(k: &Known<'_>, callee: hir_def::FunctionId) -> bool {
    let Some((clone, clone_fn)) = k.clone else { return false };
    if callee == clone_fn {
        return true;
    }
    let f = hir::Function::from(callee);
    f.name(k.db) == intern::sym::clone
        && hir::AsAssocItem::as_assoc_item(f, k.db)
            .and_then(|it| it.implemented_trait(k.db))
            .is_some_and(|it| hir_def::TraitId::from(it) == clone)
}

/// The finding of a clone of `ty`, if it is not copy-like: its message and, when the cost is
/// in a part of it, a note naming that part.
pub(super) fn finding<'db>(
    k: &Known<'db>,
    ty: &hir::Type<'db>,
) -> Option<(String, Option<String>)> {
    let costly = copy_like(k, ty, 0).err()?;
    let shown = |ty: &hir::Type<'db>| ty.display(k.db, k.target).to_string();
    let message = format!("this `clone` of `{}` {}", shown(ty), costly.why.says());
    let note = (costly.ty != *ty)
        .then(|| format!("`{}` in it {}", shown(&costly.ty), costly.why.says()));
    Some((message, note))
}

fn copy_like<'db>(k: &Known<'db>, ty: &hir::Type<'db>, depth: u8) -> Result<(), Costly<'db>> {
    let db = k.db;
    if ty.is_unknown()
        || ty.is_reference()
        || ty.is_raw_ptr()
        || ty.is_fn()
        || ty.is_never()
        || ty.as_builtin().is_some()
        || ty.is_copy(db)
    {
        return Ok(());
    }
    let costly = |why| Err(Costly { why, ty: ty.clone() });
    if depth == DEEPEST {
        return costly(Why::Deep);
    }
    let inner = ty.to_next_solver();
    if let Some(tys) = inner.as_tuple() {
        for &it in tys.as_slice() {
            copy_like(k, &ty.derived_from_next_solver(it), depth + 1)?;
        }
        return Ok(());
    }
    if let Some(element) = ty.as_array_element(db) {
        return copy_like(k, &element, depth + 1);
    }
    if ty.is_closure() {
        return costly(Why::Captures);
    }
    let Some((adt, args)) = inner.as_adt() else { return costly(Why::NotCopy) };
    if !derives_clone(k, adt) {
        return costly(hand_written(k, ty, adt));
    }
    let mut found = Ok(());
    alloc::any_variant(db, adt, |variant| {
        db.field_types(variant).iter().any(|(_, field)| {
            let field = field.ty().instantiate(k.interner, args).skip_norm_wip();
            found = copy_like(k, &ty.derived_from_next_solver(field), depth + 1);
            found.is_err()
        })
    });
    found
}

/// Whether `adt`'s `Clone` is made by `#[derive(Clone)]`: a built-in derive impl of its module.
fn derives_clone(k: &Known<'_>, adt: AdtId) -> bool {
    let db = k.db;
    let module = adt.module(db);
    let scope = module.def_map(db).module(db, module).scope(db);
    scope.builtin_derive_impls().any(|it| {
        let loc = it.loc(db);
        loc.adt == adt && matches!(loc.trait_, BuiltinDeriveImplTrait::Clone)
    })
}

/// Why the hand-written `Clone` of `ty` costs: a reference count for `Arc`, `Rc` and their
/// `Weak`s, an allocation when it reaches the allocator, else its own code.
fn hand_written<'db>(k: &Known<'db>, ty: &hir::Type<'db>, adt: AdtId) -> Why {
    let name = hir::Adt::from(adt).name(k.db);
    if matches!(name.as_str(), "Arc" | "Rc" | "Weak") {
        return Why::Counts;
    }
    let Some((_, clone_fn)) = k.clone else { return Why::HandWritten };
    let args = GenericArgs::new_from_slice(&[GenericArg::from(ty.to_next_solver())]);
    let resolved = alloc::resolve(k.db, k.env, clone_fn, args);
    match alloc::instance_reaches(k.db, k.env, resolved, true) {
        true => Why::Allocates,
        false => Why::HandWritten,
    }
}
