//! `stable_sort_needless`: a stable sort (`sort`, `sort_by`, `sort_by_key`) where stability can't
//! change the result. A stable sort keeps equal elements in their old order, which costs a
//! buffer of half the slice (an allocation) and more moves; `sort_unstable*` sorts in place and
//! is faster. When equal elements can't be told apart, both give the same slice.
//!
//! Found, on std's slice (and `Vec`) sorts:
//!
//! - `sort()` of elements whose order is total and whose equal elements are indistinguishable:
//!   `String`, `&str`, tuples and arrays of such, and structs and enums of the crate or its
//!   dependencies that `#[derive(PartialEq, Eq, PartialOrd, Ord)]` over fields that are all such
//!   (derived equality is equality of every field). Elements that are primitives (integers,
//!   `bool`, `char`, `&str`, and tuples and arrays of them) are clippy's `stable_sort_primitive`
//!   and are left to it.
//! - `sort_by_key(|x| *x)` (`x.clone()`, `x`) and `sort_by(|a, b| a.cmp(b))` (or `b.cmp(a)`): the
//!   key is the whole element, and its elements are indistinguishable as above, primitives
//!   included (clippy's lint covers `sort()` only).
//!
//! Narrowed: a sort whose result is then deduped (`sort_by_key(k)` then `dedup_by_key(k)`) is not
//! found unless the rule above holds: which of the equal elements survives the dedup is exactly
//! what stability decides. A hand-written `PartialEq`/`Ord`, a reference to anything but `str`
//! (two equal values at two addresses are distinguishable), and floats are left alone. The fix
//! renames the method: `sort` to `sort_unstable`, `sort_by` to
//! `sort_unstable_by`, `sort_by_key` to `sort_unstable_by_key`; it is not offered yet, the
//! message names the method.

use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{BindingId, Expr, ExprId, Pat, UnaryOp},
    signatures::FunctionSignature,
};
use syntax::view::{self as ast, HasAttrs as _};

use super::{Finding, Lint, alloc::written_callee, exempt, facts, hash_order_output::local_of};
use crate::check::{self, Cx};

const LINT: Lint = Lint::StableSortNeedless;

/// How many types deep an element's fields are followed.
const DEPTH: u32 = 4;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    for (id, expr) in body.exprs() {
        let Expr::MethodCall { receiver, args, .. } = expr else { continue };
        let Some((callee, _)) = written_callee(infer, body, id) else { continue };
        let name = FunctionSignature::of(db, callee).name.as_str();
        if !matches!(name, "sort" | "sort_by" | "sort_by_key") {
            continue;
        }
        let krate = hir::Function::from(callee).module(db).krate(db);
        if !["core", "alloc", "std"].iter().any(|it| facts::crate_is(db, krate, it)) {
            continue;
        }
        let Some(element) = element(cx, &analysis.ty(infer.expr_ty(receiver))) else { continue };
        let found_here = match (name, args) {
            ("sort", []) => !primitive(db, &element, DEPTH) && same_when_equal(cx, &element, DEPTH),
            ("sort_by_key", [key]) => {
                whole_key(cx, def, body, *key) && same_when_equal(cx, &element, DEPTH)
            }
            ("sort_by", [compare]) => {
                whole_compare(cx, def, body, *compare) && same_when_equal(cx, &element, DEPTH)
            }
            _ => false,
        };
        if !found_here {
            continue;
        }
        let Some(range) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), range, LINT) {
            continue;
        }
        let unstable = match name {
            "sort" => "sort_unstable",
            "sort_by" => "sort_unstable_by",
            _ => "sort_unstable_by_key",
        };
        let message = format!(
            "this `{name}` is stable, but equal elements here can't be told apart: `{unstable}` gives the same order"
        );
        let note = format!(
            "a stable sort keeps equal elements in their old order, paying a buffer of half the slice and more moves; \
             `{unstable}` sorts in place"
        );
        facts::push(&mut found, LINT, range, message, Some(note));
    }
    found
}

/// The element type of a slice, an array or a `Vec`, references peeled.
fn element<'db>(cx: &Cx<'db, '_>, ty: &hir::Type<'db>) -> Option<hir::Type<'db>> {
    let db = cx.db();
    let ty = ty.strip_references();
    if let Some(it) = ty.as_slice() {
        return Some(it);
    }
    if let Some((it, _)) = ty.as_array(db) {
        return Some(it);
    }
    let vec = cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec"))?;
    (ty.as_adt() == Some(vec)).then(|| ty.type_arguments().next()).flatten()
}

/// clippy's `is_recursively_primitive_type`: what `stable_sort_primitive` covers.
fn primitive(db: &base_db::Db, ty: &hir::Type<'_>, depth: u32) -> bool {
    if depth == 0 {
        return false;
    }
    if ty.as_builtin().is_some() {
        return true;
    }
    if let Some((inner, _)) = ty.as_reference() {
        return inner.is_str();
    }
    if let Some(inner) = ty.as_slice() {
        return primitive(db, &inner, depth - 1);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return primitive(db, &inner, depth - 1);
    }
    ty.is_tuple() && ty.tuple_fields(db).iter().all(|it| primitive(db, it, depth - 1))
}

/// Whether two elements of `ty` that compare equal can't be told apart: equality is every
/// part's, all the way down, and the order is total.
fn same_when_equal(cx: &Cx<'_, '_>, ty: &hir::Type<'_>, depth: u32) -> bool {
    let db = cx.db();
    if depth == 0 {
        return false;
    }
    if let Some(builtin) = ty.as_builtin() {
        return builtin.is_int()
            || builtin.is_uint()
            || builtin.is_char()
            || builtin.is_bool()
            || builtin.is_str();
    }
    if let Some((inner, _)) = ty.as_reference() {
        return inner.is_str();
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return same_when_equal(cx, &inner, depth - 1);
    }
    if ty.is_tuple() {
        return ty.tuple_fields(db).iter().all(|it| same_when_equal(cx, it, depth - 1));
    }
    let Some(adt) = ty.as_adt() else { return false };
    if Some(adt) == cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("String")) {
        return true;
    }
    if !derives_order(cx, adt) {
        return false;
    }
    match adt {
        hir::Adt::Struct(_) => {
            ty.fields(db).iter().all(|(_, it)| same_when_equal(cx, it, depth - 1))
        }
        hir::Adt::Enum(e) => {
            !adt.has_non_default_type_params(db)
                && e.variants(db)
                    .into_iter()
                    .all(|v| v.fields(db).iter().all(|f| same_when_equal(cx, &f.ty(db), depth - 1)))
        }
        hir::Adt::Union(_) => false,
    }
}

/// Whether `adt` derives `PartialEq`, `Eq`, `PartialOrd` and `Ord`.
fn derives_order(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let Some(source) = cx.adt_source(adt) else { return false };
    let mut names = [false; 4];
    let mut see = |attr: ast::Attr<'_>| {
        if attr.simple_name() != Some("derive") {
            return;
        }
        for (_, path) in check::derive_paths(attr) {
            let last = path.rsplit("::").next().unwrap_or(&path);
            if let Some(at) =
                ["PartialEq", "Eq", "PartialOrd", "Ord"].iter().position(|it| *it == last)
            {
                names[at] = true;
            }
        }
    };
    match source {
        ast::Adt::Struct(it) => it.attrs().for_each(&mut see),
        ast::Adt::Enum(it) => it.attrs().for_each(&mut see),
        ast::Adt::Union(it) => it.attrs().for_each(&mut see),
    }
    names.iter().all(|it| *it)
}

/// The plain parameters of the closure `closure` and its body, when every parameter is a plain binding.
fn params(body: &Body, closure: ExprId) -> Option<(Vec<BindingId>, ExprId)> {
    let Expr::Closure { args, body: inner, .. } = body.expr(closure) else { return None };
    // alloc: a closure's two parameters at most
    let mut out = Vec::new();
    for &pat in args {
        let Pat::Bind { id, subpat: None } = body.pat(pat) else { return None };
        out.push(id);
    }
    Some((out, inner))
}

/// Whether the key closure `key` returns its whole element: `|x| *x`, `|x| x.clone()`, `|x| x`.
fn whole_key(cx: &Cx<'_, '_>, def: DefWithBodyId, body: &Body, key: ExprId) -> bool {
    let Some((params, inner)) = params(body, key) else { return false };
    let [param] = params.as_slice() else { return false };
    let is_param = |it: ExprId| local_of(cx.db(), def, body, it) == Some(*param);
    match body.expr(inner) {
        Expr::Path(_) => is_param(inner),
        Expr::UnaryOp { expr, op: UnaryOp::Deref } => is_param(expr),
        Expr::MethodCall { receiver, args: [], method_name, .. } => {
            method_name.as_str() == "clone" && is_param(receiver)
        }
        _ => false,
    }
}

/// Whether the comparison closure `compare` compares its whole elements: `|a, b| a.cmp(b)` or
/// `|a, b| b.cmp(a)`.
fn whole_compare(cx: &Cx<'_, '_>, def: DefWithBodyId, body: &Body, compare: ExprId) -> bool {
    let Some((params, inner)) = params(body, compare) else { return false };
    let [a, b] = params.as_slice() else { return false };
    let Expr::MethodCall { receiver, args: [other], method_name, .. } = body.expr(inner) else {
        return false;
    };
    let local = |it: ExprId| local_of(cx.db(), def, body, it);
    let (x, y) = (local(receiver), local(*other));
    method_name.as_str() == "cmp"
        && ((x == Some(*a) && y == Some(*b)) || (x == Some(*b) && y == Some(*a)))
}
