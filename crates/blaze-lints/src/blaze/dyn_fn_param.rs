//! `dyn_fn_param`: a function of the crate that takes a closure as a trait object (`&dyn Fn(..)`,
//! `&mut dyn FnMut(..)`, `Box<dyn FnOnce(..)>`) and calls it inside a loop. Each call goes
//! through the vtable: an indirect call the compiler can't inline, so the loop can't be
//! unrolled or vectorized around it. A type parameter (`f: impl Fn(u32) -> u32`) makes the call
//! direct, at the price of one copy of the function per closure it is called with. It is
//! `dyn_in_hot_field` for parameters, and shares its reading of the type; Goose builds every
//! closure call in place, never through a table.
//!
//! Found: a parameter whose type is `&dyn F`, `&mut dyn F` or std's `Box<dyn F>` with `F` one of
//! `Fn`, `FnMut`, `FnOnce`, called (`f(..)`, `(*f)(..)`) inside a `loop`, `while` or `for` body or
//! an iterator closure of the function's own body. Left alone, since there `dyn` is likely the
//! point:
//!
//! - a `pub` function, which other crates call with any closure, and where `dyn` keeps one copy;
//! - a method of a trait or of a trait's impl, whose signature the trait fixes (and a generic
//!   method makes the trait not object safe);
//! - a function that calls itself, where a type parameter would grow a new instance per level
//!   (polymorphic recursion) or not build at all.
//!
//! Narrowed: a call made once per call of the function, which a loop elsewhere calls, is not
//! found. Code a macro wrote and test code are not checked. No fix: every caller's argument
//! changes with the type (`&f` to `f`, or `&mut f` kept).

use hir::{AsAssocItem as _, HasVisibility as _};
use hir_def::{
    DefWithBodyId,
    hir::{BindingId, Expr, Pat},
};

use super::{Finding, Lint, dyn_in_hot_field, exempt, facts, general_facts, loops};
use crate::check::Cx;

const LINT: Lint = Lint::DynFnParam;

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
    let hir::DefWithBody::Function(f) = analysis.def() else { return found };
    let Ok(def) = DefWithBodyId::try_from(analysis.def()) else { return found };
    let db = cx.db();
    if matches!(f.visibility(db), hir::Visibility::Public)
        || cx.expansion().is_some()
        || cx.is_in_test(cx.root())
    {
        return found;
    }
    if f.as_assoc_item(db).is_some_and(|it| {
        it.implemented_trait(db).is_some()
            || matches!(it.container(db), hir::AssocItemContainer::Trait(_))
    }) {
        return found;
    }
    let closures: [Option<hir::Trait>; 3] = [
        cx.lang_trait(hir::LangItem::Fn),
        cx.lang_trait(hir::LangItem::FnMut),
        cx.lang_trait(hir::LangItem::FnOnce),
    ];
    let body = analysis.body();
    let infer = analysis.infer();
    // The parameters bound by a plain name to a closure trait object, with the trait.
    // alloc: one function's few parameters
    let mut params: Vec<(BindingId, hir::Trait)> = Vec::new();
    for param in body.params.iter() {
        for pat in [param.formal, param.user_written] {
            let Pat::Bind { id, subpat: None } = body.pat(pat) else { continue };
            let ty = analysis.ty(infer.binding_ty(id));
            let Some(trait_) = dyn_in_hot_field::trait_object(db, &ty) else { continue };
            if closures.contains(&Some(trait_)) && !params.iter().any(|(it, _)| *it == id) {
                params.push((id, trait_));
            }
        }
    }
    if params.is_empty() {
        return found;
    }
    // A function that calls itself is left alone (polymorphic recursion).
    let recursive = body.exprs().any(|(id, expr)| {
        matches!(expr, Expr::Call { .. } | Expr::MethodCall { .. })
            && general_facts::callee(db, analysis, id)
                .is_some_and(|it| hir::Function::from(it.f) == f)
    });
    if recursive {
        return found;
    }
    let parents = facts::Parents::of(body);
    for (id, expr) in body.exprs() {
        let Expr::Call { callee, .. } = expr else { continue };
        let called = dyn_in_hot_field::peel(body, callee);
        let Some(local) = super::hash_order_output::local_of(db, def, body, called) else {
            continue;
        };
        let Some(&(_, trait_)) = params.iter().find(|(it, _)| *it == local) else { continue };
        if loops::enclosing_loop(cx, analysis, &parents, id).is_none() {
            continue;
        }
        let Some(at) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), at, LINT) {
            continue;
        }
        let name = body[local].name.as_str();
        let message = format!(
            "`{}` takes `{name}` as a `dyn {}` and calls it inside a loop: every call goes through the vtable",
            f.name(db).as_str(),
            trait_.name(db).as_str(),
        );
        let note = format!(
            "an indirect call can't be inlined, so the loop around it can't be unrolled or vectorized; `{name}: impl {}(..)` \
             (a type parameter) makes the call direct, at the price of one copy of `{}` for each closure it is called with",
            trait_.name(db).as_str(),
            f.name(db).as_str(),
        );
        facts::push(&mut found, LINT, at, message, Some(note));
        // One finding per parameter is enough.
        params.retain(|(it, _)| *it != local);
    }
    found
}
