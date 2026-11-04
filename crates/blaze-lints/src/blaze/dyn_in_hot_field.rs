//! `dyn_in_hot_field`: a struct field holding a trait object (`Box<dyn Trait>`, `&dyn Trait`)
//! that the struct's own methods call inside a loop. Each call goes through the vtable: an
//! indirect call the compiler can't inline, so the loop around it can't be unrolled, vectorized
//! or have work hoisted out of it. A type parameter (`struct S<F: Fn(u32) -> u32> { f: F }`)
//! makes the call direct and inlinable, at the price of one copy of the struct's code per type
//! it is used with.
//!
//! Found: a field of the crate's struct whose type is `Box<dyn Trait>` (std's `Box`) or
//! `&dyn Trait`, and a method of an impl of that struct that calls it, `(self.f)(..)` or
//! `self.f.method(..)`, inside a `loop`, `while` or `for` body or an iterator closure. Left
//! alone, since there a trait object is likely the point:
//!
//! - a `pub` struct, which other crates may build with any implementation;
//! - a struct that implements a trait of the workspace (it may itself be used as `dyn`);
//! - a struct the crate holds in a collection: as an element of a slice, an array, or a type
//!   argument of anything but `Box`, `Option`, `Rc`, `Arc`, `Cell`, `RefCell`, `Mutex` and
//!   `RwLock`, in a field or a function's signature anywhere in the crate. Elements of one
//!   collection must share one type, so a generic would force every element to the same
//!   implementation.
//!
//! Narrowed: the collection check reads declarations (fields and signatures), not locals, so a
//! `Vec` of the struct that only lives in a function body is missed and the lint can fire; a
//! call made once per call of a method that a loop elsewhere calls is not found. Code a macro
//! wrote and test code are not checked. No fix: the type parameter reaches every place the
//! struct is named.

use hir::HasVisibility as _;
use hir_def::{
    DefWithBodyId,
    expr_store::Body,
    hir::{Expr, ExprId, UnaryOp},
};
use syntax::view::{self as ast, AstNode as _};

use super::{Lint, facts, frozen_collection, loops};
use crate::check::Cx;

const LINT: Lint = Lint::DynInHotField;

/// How many types deep a declaration's type is searched for the struct.
const DEPTH: u32 = 4;

/// How many modules of the crate the collection check reads before giving up (and not firing).
const MODULES: usize = 4096;

/// Types that hold one value, not a collection of them.
const WRAPPERS: &[&str] = &["Box", "Option", "Rc", "Arc", "Cell", "RefCell", "Mutex", "RwLock"];

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, item: ast::Struct<'t>) {
    if !cx.on_blaze(LINT)
        || cx.expansion().is_some()
        || !cx.is_written(item.syntax())
        || cx.is_in_test(item.syntax())
    {
        return;
    }
    let Some(hir::Adt::Struct(def)) = cx.adt_def(ast::Adt::Struct(item)) else { return };
    let db = cx.db();
    if matches!(def.visibility(db), hir::Visibility::Public) {
        return;
    }
    let adt = hir::Adt::Struct(def);
    // alloc: the few trait-object fields of one struct
    let mut fields = Vec::new();
    for (index, field) in def.fields(db).into_iter().enumerate() {
        if let Some(trait_) = trait_object(db, &field.ty(db)) {
            fields.push((index, field, trait_));
        }
    }
    if fields.is_empty() {
        return;
    }
    let impls = hir::Impl::all_for_type(db, cx.krate(), adt.ty(db));
    let local_trait = impls
        .iter()
        .any(|imp| imp.trait_(db).is_some_and(|it| it.module(db).krate(db).origin(db).is_local()));
    if local_trait || in_collection(cx, adt) {
        return;
    }
    let name = def.name(db);
    for (index, field, trait_) in fields {
        let field_name = field.name(db);
        let Some(method) = impls.iter().find_map(|imp| {
            imp.items(db)
                .into_iter()
                .filter_map(|it| it.as_function())
                .find(|f| calls_in_loop(cx, adt, &field_name, *f))
        }) else {
            continue;
        };
        let Some(node) = frozen_collection::field_node(item, &field_name, index) else { continue };
        let Some(range) = cx.range(node) else { continue };
        let message = format!(
            "the field `{}` of `{}` is a `dyn {}` that `{}` calls inside a loop: every call goes through the vtable",
            field_name.as_str(),
            name.as_str(),
            trait_.name(db).as_str(),
            method.name(db).as_str()
        );
        let note = format!(
            "an indirect call can't be inlined, so the loop around it can't be unrolled or vectorized; a type parameter \
             (`struct {}<F: {}> {{ {}: F, .. }}`) makes the call direct, at the price of one copy of the struct's code \
             for each type it is used with (larger binaries and longer builds)",
            name.as_str(),
            trait_.name(db).as_str(),
            field_name.as_str()
        );
        let span = cx.span(range);
        cx.blaze(LINT, node, span, message)
            .note(note)
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url()));
    }
}

/// The trait of `ty` when it is `Box<dyn Trait>` (std's `Box`) or `&dyn Trait`. Shared with
/// `dyn_fn_param`, which asks the same of a parameter.
pub(super) fn trait_object(db: &base_db::Db, ty: &hir::Type<'_>) -> Option<hir::Trait> {
    if let Some((inner, _)) = ty.as_reference() {
        return inner.as_dyn_trait();
    }
    let adt = ty.as_adt()?;
    if !(facts::adt_is(db, adt, "alloc", "Box") || facts::adt_is(db, adt, "std", "Box")) {
        return None;
    }
    ty.type_arguments().next()?.as_dyn_trait()
}

/// Whether the method `f` calls the field `field` of `adt` inside a loop.
fn calls_in_loop(cx: &Cx<'_, '_>, adt: hir::Adt, field: &hir::Name, f: hir::Function) -> bool {
    let db = cx.db();
    let owner = hir::DefWithBody::from(f);
    if DefWithBodyId::try_from(owner).is_err() {
        return false;
    }
    let analysis = hir::BodyAnalysis::new(db, owner);
    let body = analysis.body();
    let infer = analysis.infer();
    let is_field = |at: ExprId| {
        let Expr::Field { expr: of, name } = body.expr(peel(body, at)) else { return false };
        name == field && analysis.ty(infer.expr_ty(of)).strip_references().as_adt() == Some(adt)
    };
    let mut parents = None;
    for (id, expr) in body.exprs() {
        let called = match expr {
            Expr::Call { callee, .. } => is_field(callee),
            Expr::MethodCall { receiver, .. } => is_field(receiver),
            _ => false,
        };
        if !called {
            continue;
        }
        let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
        if loops::enclosing_loop(cx, &analysis, parents, id).is_some() {
            return true;
        }
    }
    false
}

/// `expr` with its borrows and derefs peeled.
pub(super) fn peel(body: &Body, expr: ExprId) -> ExprId {
    let mut at = expr;
    for _ in 0..8 {
        match body.expr(at) {
            Expr::Ref { expr, .. } | Expr::UnaryOp { expr, op: UnaryOp::Deref } => at = expr,
            _ => break,
        }
    }
    at
}

/// Whether the crate holds `target` as an element of a collection, in a field or a signature.
/// A crate too big to read whole counts as holding it, so the lint stays quiet.
fn in_collection(cx: &Cx<'_, '_>, target: hir::Adt) -> bool {
    let db = cx.db();
    let holds = |ty: &hir::Type<'_>| holds(db, ty, target, false, DEPTH);
    let in_fn = |f: hir::Function| {
        f.assoc_fn_params(db).iter().any(|it| holds(it.ty())) || holds(&f.ret_type(db))
    };
    // alloc: the worklist of one crate's modules
    let mut modules = vec![cx.krate().root_module(db)];
    let mut seen = 0;
    while let Some(module) = modules.pop() {
        seen += 1;
        if seen > MODULES {
            return true;
        }
        modules.extend(module.children(db));
        for def in module.declarations(db) {
            let held = match def {
                hir::ModuleDef::Static(it) => holds(&it.ty(db)),
                hir::ModuleDef::Const(it) => holds(&it.ty(db)),
                hir::ModuleDef::Adt(it) => {
                    let fields = match it {
                        hir::Adt::Struct(s) => s.fields(db),
                        hir::Adt::Union(u) => u.fields(db),
                        // alloc: one enum's fields
                        hir::Adt::Enum(e) => {
                            e.variants(db).into_iter().flat_map(|v| v.fields(db)).collect()
                        }
                    };
                    fields.iter().any(|field| holds(&field.ty(db)))
                }
                hir::ModuleDef::Function(f) => in_fn(f),
                _ => false,
            };
            if held {
                return true;
            }
        }
        for imp in module.impl_defs(db) {
            if holds(&imp.self_ty(db)) {
                return true;
            }
            for item in imp.items(db) {
                if let hir::AssocItem::Function(f) = item
                    && in_fn(f)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether `ty` holds `target` as an element: inside a slice, an array, or a type argument of an
/// ADT that is not one of [`WRAPPERS`], through references, tuples and wrappers, at most
/// `depth` types deep. `many` says a collection was passed on the way.
fn holds(db: &base_db::Db, ty: &hir::Type<'_>, target: hir::Adt, many: bool, depth: u32) -> bool {
    if depth == 0 {
        return false;
    }
    let ty = ty.strip_references();
    if let Some(inner) = ty.as_slice() {
        return holds(db, &inner, target, true, depth - 1);
    }
    if let Some((inner, _)) = ty.as_array(db) {
        return holds(db, &inner, target, true, depth - 1);
    }
    if ty.is_tuple() {
        return ty.tuple_fields(db).iter().any(|it| holds(db, it, target, many, depth - 1));
    }
    let Some(adt) = ty.as_adt() else { return false };
    if adt == target {
        return many;
    }
    let wrapper = WRAPPERS.contains(&adt.name(db).as_str())
        && ["core", "alloc", "std"]
            .iter()
            .any(|it| facts::crate_is(db, adt.module(db).krate(db), it));
    let many = many || !wrapper;
    ty.type_arguments().any(|it| holds(db, &it, target, many, depth - 1))
}
