//! rustc's `Ty::sort_string`/`prefix_string` and `string_with_limit`, which the type error
//! messages are made of, over the one printer of `hir_ty_print`. Ported from
//! `compiler/rustc_middle/src/ty/error.rs` and
//! `compiler/rustc_trait_selection/src/error_reporting/infer/mod.rs` (MIT or Apache-2.0).

use hir_def::AdtId;
use hir_ty::{
    db::InternedOpaqueTyId,
    next_solver::{CallableIdWrapper, ClauseKind, Ty, TyKind},
};
use hir_ty_print::{Def, Mode, Printer, coroutine_desc};
use rustc_type_ir::{
    AliasTyKind, InferTy, Mutability,
    inherent::{BoundExistentialPredicates as _, IntoKind as _},
};

use crate::cx::Cx;

/// `ty` printed by its own printer in `mode`.
pub(super) fn string(cx: &Cx<'_, '_>, ty: Ty<'_>, mode: Mode) -> String {
    let mut p = Printer::diagnostic(cx.db, cx, mode, false);
    p.print_type(ty);
    p.finish()
}

/// `def`'s path, as `def_path_str` prints it, in `mode`.
fn path_string(cx: &Cx<'_, '_>, def: Def, mode: Mode) -> String {
    let mut p = Printer::diagnostic(cx.db, cx, mode, def.is_value());
    p.print_def_path(def, &[]);
    p.finish()
}

/// rustc's `string_with_limit`: the type if it is at most `limit` bytes long, else forced
/// trimmed with ever fewer types until it is.
pub(super) fn string_with_limit(cx: &Cx<'_, '_>, ty: Ty<'_>, limit: usize, mode: Mode) -> String {
    let regular = string(cx, ty, mode);
    if regular.len() <= limit {
        return regular;
    }
    let mut type_limit = 50;
    loop {
        let short = string(cx, ty, mode.forced().limited(type_limit));
        if short.len() <= limit || type_limit == 0 {
            return short;
        }
        type_limit -= 1;
    }
}

/// rustc's `Ty::sort_string`: what kind of type `ty` is, or the type in backticks, at most
/// `max(diagnostic_width / 4, 40)` bytes long with rustc's default width.
pub(super) fn sort_string(cx: &Cx<'_, '_>, ty: Ty<'_>, mode: Mode) -> String {
    let db = cx.db;
    let s = match ty.kind() {
        TyKind::Foreign(it) => {
            // alloc: a type's sort printed, the edge
            return format!("extern type `{}`", path_string(cx, Def::TypeAlias(it.0), mode));
        }
        TyKind::FnDef(def, _) => fn_def_kind(def),
        TyKind::FnPtr(..) => "fn pointer",
        TyKind::Dynamic(preds, _) if let Some(principal) = preds.principal() => {
            // alloc: a type's sort printed, the edge
            return format!(
                "`dyn {}`",
                path_string(cx, Def::Trait(principal.skip_binder().def_id.0), mode)
            );
        }
        TyKind::Dynamic(..) => "trait object",
        TyKind::Closure(..) => "closure",
        TyKind::Coroutine(id, _) => return coroutine_desc(id.0.loc(db).kind, true),
        TyKind::CoroutineWitness(..) => "coroutine witness",
        TyKind::Infer(InferTy::TyVar(_)) => "inferred type",
        TyKind::Infer(InferTy::IntVar(_)) => "integer",
        TyKind::Infer(InferTy::FloatVar(_)) => "floating-point number",
        TyKind::Placeholder(..) => "placeholder type",
        TyKind::Bound(..) => "bound type",
        TyKind::Infer(InferTy::FreshTy(_)) => "fresh type",
        // hir's types as inference had them at an error hold an undecided literal's as these.
        TyKind::Infer(InferTy::FreshIntTy(_)) => "integer",
        TyKind::Infer(InferTy::FreshFloatTy(_)) => "floating-point number",
        TyKind::Alias(_, alias)
            if matches!(
                alias.kind,
                AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }
            ) =>
        {
            "associated type"
        }
        // alloc: a type's sort printed, the edge
        TyKind::Param(_) => return format!("type parameter `{}`", string(cx, ty, mode)),
        TyKind::Alias(_, alias) if let AliasTyKind::Opaque { def_id } = alias.kind => {
            if is_future(cx, def_id.0) { "future" } else { "opaque type" }
        }
        TyKind::Error(_) => "type error",
        // alloc: a type's sort printed, the edge
        _ => return format!("`{}`", string_with_limit(cx, ty, 40, mode)),
    };
    s.to_owned() // alloc: a type's sort printed, the edge
}

fn fn_def_kind(def: CallableIdWrapper) -> &'static str {
    match def.0 {
        hir_def::CallableDefId::StructId(_) => "struct constructor",
        hir_def::CallableDefId::EnumVariantId(_) => "enum constructor",
        hir_def::CallableDefId::FunctionId(_) => "fn item",
    }
}

/// rustc's `Ty::prefix_string`: the kind of type `ty` is, for `expected X, found Y`.
pub(super) fn prefix_string(cx: &Cx<'_, '_>, ty: Ty<'_>) -> String {
    let db = cx.db;
    let s = match ty.kind() {
        TyKind::Infer(_)
        | TyKind::Error(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never => "type",
        TyKind::Tuple(tys) if tys.as_slice().is_empty() => "unit type",
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::StructId(_) => "struct",
            AdtId::UnionId(_) => "union",
            AdtId::EnumId(_) => "enum",
        },
        TyKind::Foreign(_) => "extern type",
        TyKind::Array(..) => "array",
        TyKind::Pat(..) => "pattern type",
        TyKind::Slice(_) => "slice",
        TyKind::RawPtr(..) => "raw pointer",
        TyKind::Ref(_, _, Mutability::Mut) => "mutable reference",
        TyKind::Ref(..) => "reference",
        TyKind::FnDef(def, _) => fn_def_kind(def),
        TyKind::FnPtr(..) => "fn pointer",
        TyKind::UnsafeBinder(_) => "unsafe binder",
        TyKind::Dynamic(..) => "trait object",
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => "closure",
        TyKind::Coroutine(id, _) => return coroutine_desc(id.0.loc(db).kind, true),
        TyKind::CoroutineWitness(..) => "coroutine witness",
        TyKind::Tuple(_) => "tuple",
        TyKind::Placeholder(_) => "higher-ranked type",
        TyKind::Bound(..) => "bound type variable",
        TyKind::Alias(_, alias) => match alias.kind {
            AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. } => "associated type",
            AliasTyKind::Free { .. } => "type alias",
            AliasTyKind::Opaque { .. } => "opaque type",
        },
        TyKind::Param(_) => "type parameter",
    };
    s.to_owned() // alloc: a type's kind printed, the edge
}

/// Whether an opaque type is bounded by `Future`: rustc's `is_impl_trait_future`.
fn is_future(cx: &Cx<'_, '_>, id: InternedOpaqueTyId<'_>) -> bool {
    let db = cx.db;
    let Some(future) = hir_def::lang_item::lang_items(db, cx.krate.into()).Future else {
        return false;
    };
    id.self_predicates(db).skip_binder().iter().any(|clause| {
        matches!(clause.kind().skip_binder(), ClauseKind::Trait(it) if it.def_id().0 == future)
    })
}
