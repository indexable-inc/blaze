//! `missing_size_hint`: an `impl Iterator` for a struct of the crate that knows how many items
//! are left and doesn't say. `Iterator::size_hint` defaults to `(0, None)`, so `collect`,
//! `extend` and `Vec::from_iter` grow the buffer by doubling (several reallocations and copies)
//! instead of allocating once, and adaptors such as `zip`, `chain` and `rev` lose the length.
//! `ExactSizeIterator` adds `len()` and lets `TrustedLen`-style specializations fire.
//!
//! - **No `size_hint`**: `next` steps a counter field of the struct (`self.i += 1`, `self.n -=
//!   1`) and the struct holds what the counter walks: a slice, a `Vec`, an array, a boxed
//!   slice, or a second integer field (a length or an end). The remaining count is then a
//!   subtraction away.
//! - **An exact `size_hint`**: `size_hint` returns `(n, Some(n))` with the same `n`, and the
//!   type has no `ExactSizeIterator` impl.
//!
//! Only structs defined in the crate, impls written in the file (not a macro's) and code outside
//! tests are checked; a blanket `impl<T> Iterator for T` has no struct and is skipped. No fix:
//! the count's formula is the type's own.

use hir_def::{
    DefWithBodyId,
    hir::{ArithOp, BinaryOp, Expr, ExprId},
};
use syntax::view::{self as ast, AstNode as _};

use super::{Lint, hash_order_output::local_of};
use crate::check::Cx;

const LINT: Lint = Lint::MissingSizeHint;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Impl<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(imp) = cx.impl_def(item) else { return };
    let db = cx.db();
    let Some(iterator) = cx.lang_trait(hir::LangItem::Iterator) else { return };
    if imp.trait_(db) != Some(iterator) {
        return;
    }
    let self_ty = imp.self_ty(db);
    let Some(hir::Adt::Struct(adt)) = self_ty.as_adt() else { return };
    if adt.module(db).krate(db) != cx.krate() {
        return;
    }
    let named = |name: &str| {
        imp.items(db)
            .into_iter()
            .filter_map(|it| it.as_function())
            .find(|it| it.name(db).as_str() == name)
    };
    let Some(next) = named("next") else { return };
    let ty_name = adt.name(db);
    let (message, note) = match named("size_hint") {
        None => {
            let Some((counter, walked)) = walks(cx, adt, next) else { return };
            (
                format!(
                    "`impl Iterator for {}` has no `size_hint`, though `next` steps the counter `{counter}` over {walked}",
                    ty_name.as_str()
                ),
                "the default `size_hint` is `(0, None)`: `collect` and `extend` then grow their buffer by doubling \
                 instead of allocating once, and `zip`, `chain` and `rev` lose the length; return the items left, \
                 `(n, Some(n))`, and implement `ExactSizeIterator` when `n` is exact"
                    .to_owned(), // alloc: a message's part
            )
        }
        Some(hint) => {
            if !exact(db, hint) || impls_exact(db, cx.krate(), &self_ty) {
                return;
            }
            (
                format!(
                    "`{}`'s `size_hint` returns an exact `(n, Some(n))` but the type does not implement \
                     `ExactSizeIterator`",
                    ty_name.as_str()
                ),
                "`impl ExactSizeIterator for ..` (empty: `len` comes from `size_hint`) gives callers `len()` and \
                 lets std's exact-size paths run"
                    .to_owned(), // alloc: a message's part
            )
        }
    };
    let at = item.self_ty().map_or(item.syntax(), |it| it.syntax());
    let Some(range) = cx.range(at) else { return };
    let span = cx.span(range);
    cx.blaze(LINT, item.syntax(), span, message)
        .note(note)
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// What `next` of `adt` steps and walks: the counter field it adds to or subtracts from, and
/// what else the struct holds for it to walk.
fn walks(cx: &Cx<'_, '_>, adt: hir::Struct, next: hir::Function) -> Option<(String, &'static str)> {
    let db = cx.db();
    let fields = adt.fields(db);
    let int = |field: &hir::Field| field.ty(db).is_int_or_uint();
    let vec = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec"));
    let sequence = fields.iter().any(|field| {
        let ty = field.ty(db).strip_references();
        let boxed_slice = ty.as_adt().is_some_and(|it| it.name(db).as_str() == "Box")
            && ty.type_arguments().next().is_some_and(|it| it.is_slice());
        ty.is_slice()
            || ty.is_array()
            || (ty.as_adt().is_some() && ty.as_adt() == vec)
            || boxed_slice
    });
    let ints = fields.iter().filter(|it| int(it)).count();
    let owner = hir::DefWithBody::from(next);
    DefWithBodyId::try_from(owner).ok()?;
    let analysis = hir::BodyAnalysis::new(db, owner);
    let body = analysis.body();
    let infer = analysis.infer();
    let counter = body.exprs().find_map(|(_, expr)| {
        let Expr::BinaryOp {
            lhs,
            op: Some(BinaryOp::Assignment { op: Some(ArithOp::Add | ArithOp::Sub) }),
            ..
        } = expr
        else {
            return None;
        };
        let Expr::Field { expr: of, name } = body.expr(lhs) else { return None };
        if analysis.ty(infer.expr_ty(of)).strip_references().as_adt() != Some(hir::Adt::Struct(adt))
        {
            return None;
        }
        let field = fields.iter().find(|it| it.name(db) == *name)?;
        int(field).then(|| name.as_str().to_owned()) // alloc: a message's part
    })?;
    match (sequence, ints >= 2) {
        (true, _) => Some((counter, "the slice it holds")),
        (false, true) => Some((counter, "a range up to another of its fields")),
        (false, false) => None,
    }
}

/// Whether `hint`'s body ends in `(n, Some(n))`, `n` the same local or field both times.
fn exact(db: &base_db::Db, hint: hir::Function) -> bool {
    let owner = hir::DefWithBody::from(hint);
    let Ok(def) = DefWithBodyId::try_from(owner) else { return false };
    let analysis = hir::BodyAnalysis::new(db, owner);
    let body = analysis.body();
    let mut at = body.root_expr();
    for _ in 0..4 {
        match body.expr(at) {
            Expr::Block { tail: Some(tail), .. } => at = tail,
            Expr::Return { expr: Some(expr) } => at = expr,
            _ => break,
        }
    }
    let Expr::Tuple { exprs: [low, high] } = body.expr(at) else { return false };
    let Expr::Call { args: [inner], .. } = body.expr(*high) else { return false };
    let option = analysis
        .ty(analysis.infer().expr_ty(*high))
        .as_adt()
        .is_some_and(|it| it.name(db).as_str() == "Option");
    option && same(db, def, body, *low, *inner)
}

/// Whether `a` and `b` are the same place: one local, or the same field of one.
fn same(
    db: &base_db::Db,
    def: DefWithBodyId,
    body: &hir_def::expr_store::Body,
    a: ExprId,
    b: ExprId,
) -> bool {
    match (body.expr(a), body.expr(b)) {
        (Expr::Path(_), Expr::Path(_)) => {
            let local = local_of(db, def, body, a);
            local.is_some() && local == local_of(db, def, body, b)
        }
        (Expr::Field { expr: x, name: m }, Expr::Field { expr: y, name: n }) => {
            m == n && same(db, def, body, x, y)
        }
        _ => false,
    }
}

/// Whether `ty` has an `ExactSizeIterator` impl `krate` sees.
fn impls_exact(db: &base_db::Db, krate: hir::Crate, ty: &hir::Type<'_>) -> bool {
    // clone: the type handed to the impl search, once per iterator impl
    hir::Impl::all_for_type(db, krate, ty.clone())
        .into_iter()
        .any(|imp| imp.trait_(db).is_some_and(|it| it.name(db).as_str() == "ExactSizeIterator"))
}
