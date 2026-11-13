//! What the prover knows of a few std functions, by what they are (their crate, the type or
//! trait they are a method of, their name), never by a body: what `len`, `is_some`, `first`,
//! a range's `next` return, and when `unwrap` and an index panic
//! (docs/v2/plan/design/proved-unreachable.md, tier 2).
//!
//! This table is trusted, as `effect_attrs::ROWS` is. When contracts land on `library/`:
//! each row becomes the `requires`/`ensures` on its function and this file goes.

use base_db::{CrateOrigin, Db, LangCrateOrigin};
use hir_def::{
    AdtId, EnumVariantId, FunctionId, HasModule, ItemContainerId, Lookup as _,
    signatures::{EnumSignature, FunctionSignature, StructSignature, TraitSignature},
};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{GenericArgs, Ty, TyKind},
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind as _};

/// What a call of a modeled function is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Model {
    /// `len(&self)`: the length of what `self` leads to.
    Len,
    /// `is_empty(&self)`: whether that length is 0.
    IsEmpty,
    /// `is_some`, `is_none`, `is_ok`, `is_err`: whether the discriminant is (`is`) or is not
    /// `discr`.
    IsVariant { discr: i128, is: bool },
    /// `unwrap`, `expect` and their `_err` forms: panics unless the discriminant is `discr`,
    /// and returns that variant's field.
    Unwrap { discr: i128 },
    /// `index(&self, i: usize)`, `index_mut`: panics unless `i < len`.
    Index,
    /// `first(&self)`, `last(&self)`: `Some` exactly when the length is not 0.
    First,
    /// `get(&self, i: usize)`: `Some` exactly when `i < len`.
    Get,
    /// `NonZero::get(self)`: not 0.
    NonZeroGet,
    /// `Iterator::next` of a `Range` of integers: `Some(x)` implies `start <= x < end`, and
    /// the range's end stays.
    RangeNext,
    /// `IntoIterator::into_iter` of an iterator: itself.
    Itself,
    /// `Deref::deref` and `as_slice` of a `Vec`, `as_str` of a `String`: a view of the same
    /// length.
    View,
}

/// The type a method is of, as the models tell types apart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Head {
    Slice,
    Array,
    Str,
    Vec,
    String,
    Option,
    Result,
    NonZero,
    Range,
}

/// `ty` with its references taken off, when it is a type the models know: a builtin, or a
/// type of that name in the toolchain's crate it is defined in.
fn head(db: &Db, mut ty: Ty<'_>) -> Option<Head> {
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
    }
    let (adt, args) = match ty.kind() {
        TyKind::Slice(_) => return Some(Head::Slice),
        TyKind::Array(..) => return Some(Head::Array),
        TyKind::Str => return Some(Head::Str),
        TyKind::Adt(def, args) => (def.def_id(), args),
        _ => return None,
    };
    let origin = match &HasModule::krate(&adt, db).data(db).origin {
        CrateOrigin::Lang(origin) => *origin,
        _ => return None,
    };
    let (core, alloc) = (
        matches!(origin, LangCrateOrigin::Core),
        matches!(origin, LangCrateOrigin::Alloc),
    );
    Some(match adt {
        AdtId::EnumId(it) => match EnumSignature::of(db, it).name.symbol().as_str() {
            "Option" if core => Head::Option,
            "Result" if core => Head::Result,
            _ => return None,
        },
        AdtId::StructId(it) => match StructSignature::of(db, it).name.symbol().as_str() {
            "Vec" if alloc => Head::Vec,
            "String" if alloc => Head::String,
            "NonZero" if core => Head::NonZero,
            // A range of integers only: its `next` is the one the model states.
            "Range" if core && args.len() == 1 && is_integer(args.type_at(0)) => Head::Range,
            _ => return None,
        },
        AdtId::UnionId(_) => return None,
    })
}

fn is_integer(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
}

/// The model of a call of `func` with `args`, when it has one.
pub(crate) fn model_of<'db>(
    db: &'db Db,
    func: FunctionId,
    args: GenericArgs<'db>,
) -> Option<Model> {
    if !matches!(HasModule::krate(&func, db).data(db).origin, CrateOrigin::Lang(_)) {
        return None;
    }
    let name = FunctionSignature::of(db, func).name.symbol().as_str();
    // The trait the method is of, and the type it is a method of.
    let (trait_, self_ty) = match func.lookup(db).container {
        ItemContainerId::ImplId(imp) => (
            db.impl_trait(imp).map(|it| it.skip_binder().def_id.0),
            db.impl_self_ty(imp).skip_binder(),
        ),
        ItemContainerId::TraitId(it) => {
            if args.len() == 0 {
                return None;
            }
            (Some(it), args.type_at(0))
        }
        _ => return None,
    };
    let head = head(db, self_ty);
    let Some(trait_) = trait_ else {
        return match (head?, name) {
            (Head::Slice | Head::Str | Head::Vec | Head::String, "len") => Some(Model::Len),
            (Head::Slice | Head::Str | Head::Vec | Head::String, "is_empty") => {
                Some(Model::IsEmpty)
            }
            (Head::Slice, "first" | "last") => Some(Model::First),
            (Head::Slice, "get") => Some(Model::Get),
            (Head::Vec, "as_slice") | (Head::String, "as_str") => Some(Model::View),
            (Head::Option, "is_some") => Some(Model::IsVariant { discr: 1, is: true }),
            (Head::Option, "is_none") => Some(Model::IsVariant { discr: 1, is: false }),
            (Head::Result, "is_ok") => Some(Model::IsVariant { discr: 0, is: true }),
            (Head::Result, "is_err") => Some(Model::IsVariant { discr: 0, is: false }),
            (Head::Option, "unwrap" | "expect") => Some(Model::Unwrap { discr: 1 }),
            (Head::Result, "unwrap" | "expect") => Some(Model::Unwrap { discr: 0 }),
            (Head::Result, "unwrap_err" | "expect_err") => Some(Model::Unwrap { discr: 1 }),
            (Head::NonZero, "get") => Some(Model::NonZeroGet),
            _ => None,
        };
    };
    if !matches!(
        HasModule::krate(&trait_, db).data(db).origin,
        CrateOrigin::Lang(LangCrateOrigin::Core)
    ) {
        return None;
    }
    match (TraitSignature::of(db, trait_).name.symbol().as_str(), name, head) {
        ("Index", "index", Some(Head::Slice | Head::Array | Head::Vec))
        | ("IndexMut", "index_mut", Some(Head::Slice | Head::Array | Head::Vec)) => {
            Some(Model::Index)
        }
        ("Deref", "deref", Some(Head::Vec | Head::String))
        | ("DerefMut", "deref_mut", Some(Head::Vec | Head::String)) => Some(Model::View),
        ("Iterator", "next", Some(Head::Range)) => Some(Model::RangeNext),
        // Every iterator's `into_iter` is the blanket impl's, which returns it.
        ("IntoIterator", "into_iter", Some(Head::Range)) => Some(Model::Itself),
        _ => None,
    }
}

/// The values a discriminant of the enum type `ty` has, as a range, when every variant's is
/// known and inside `0..variants`: `Option`'s and `Result`'s 0 and 1, a plain enum's indices.
pub(crate) fn discriminants(db: &Db, ty: Ty<'_>) -> Option<(i128, i128)> {
    let TyKind::Adt(def, _) = ty.kind() else { return None };
    let AdtId::EnumId(it) = def.def_id() else { return None };
    let variants = &it.enum_variants(db).variants;
    let count = variants.len();
    if count == 0 || count > 16 {
        return None;
    }
    for (variant, _) in variants.values() {
        let value = db.const_eval_discriminant(*variant).ok()?;
        if value < 0 || value >= count as i128 {
            return None;
        }
    }
    Some((0, count as i128 - 1))
}

/// The variant of the enum type `ty` whose discriminant is `discr`: `Some` of an `Option`
/// for 1, `Ok` of a `Result` for 0.
pub(crate) fn variant_at(db: &Db, ty: Ty<'_>, discr: i128) -> Option<EnumVariantId> {
    let TyKind::Adt(def, _) = ty.kind() else { return None };
    let AdtId::EnumId(it) = def.def_id() else { return None };
    it.enum_variants(db)
        .variants
        .values()
        .map(|(variant, _)| *variant)
        .find(|variant| db.const_eval_discriminant(*variant).ok() == Some(discr))
}
