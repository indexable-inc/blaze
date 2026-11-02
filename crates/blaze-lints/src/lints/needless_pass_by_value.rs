//! `needless_pass_by_value`: a function taking by value an argument that its body never
//! consumes, which a reference would serve.
//!
//! Follows clippy's `clippy_lints/src/needless_pass_by_value.rs` (`NeedlessPassByValue::check_fn`)
//! and `clippy_utils::ptr::get_spans` (MIT or Apache-2.0), ported from what is known of their
//! source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy was at hand.
//! What the body consumes is the front end's `ExprUseVisitor` (`BodyAnalysis::consumes`), as
//! clippy's `MovedVariablesCtxt` asks rustc's.

use hir::{LangItem, PathResolution};
use syntax::{
    TextRange,
    view::{
        self as ast, AstNode as _, HasArgList as _, HasAttrs as _, HasGenericArgs as _, HasName as _,
    },
};

use super::large_types_passed_by_value::{Owner, foreign_abi, owner};
use crate::{
    check::Cx,
    diag::{Applicability, Span, SuggestionStyle},
};

const LINT: super::Implemented = super::Implemented::NeedlessPassByValue;

/// How many macro calls deep the search for `.clone()` goes into expansions.
const DEEPEST: u32 = 8;

/// A parameter that may be linted, by what is written.
struct Candidate<'t> {
    /// Its index among the signature's inputs, `self` included.
    input: usize,
    pat: ast::IdentPat<'t>,
    ty: ast::Type<'t>,
}

/// Run once per body, with the function that owns it.
pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(function) = ast::Fn::cast(node) else { return };
    let (Some(params), Some(body)) = (function.param_list(), function.body()) else { return };
    // `self` is never linted.
    let first = usize::from(params.self_param().is_some());
    let mut candidates = Vec::new();
    for (at, param) in params.params().enumerate() {
        let (Some(ast::Pat::IdentPat(pat)), Some(ty)) = (param.pat(), param.ty()) else { continue };
        if pat.name().is_none_or(|it| it.text().starts_with('_')) {
            continue;
        }
        // `BindingMode(_, Mutability::Not)`: `ref` and `ref mut` bind no `mut` name.
        if pat.mut_token().is_some() && pat.ref_token().is_none() {
            continue;
        }
        if by_value(ty) {
            candidates.push(Candidate { input: first + at, pat, ty });
        }
    }
    if candidates.is_empty() || !cx.is_written(node) || cx.range(node).is_none() {
        return;
    }
    match owner(function) {
        Owner::Trait | Owner::TraitImpl => return,
        Owner::Item if foreign_abi(function) || requires_exact_signature(function) => return,
        _ => {}
    }
    let db = cx.db();
    let Some(def) = cx.fn_def(function) else { return };
    let Some(analysis) = cx.analysis() else { return };
    if analysis.def() != hir::DefWithBody::Function(def) {
        return;
    }
    // clippy's `need!`: without these, it lints nothing.
    let lang = [LangItem::Fn, LangItem::FnOnce, LangItem::FnMut, LangItem::Sized, LangItem::MetaSized];
    if lang.iter().any(|it| cx.lang_trait(*it).is_none()) {
        return;
    }
    let Some(range_bounds) = diagnostic_trait(cx, "RangeBounds") else { return };
    let borrow = diagnostic_trait(cx, "Borrow");
    let Some(signature) = def.signature(db) else { return };
    let types = signature.params(db);
    let mut found: Vec<(Candidate<'t>, hir::Type<'_>)> = Vec::new();
    for candidate in candidates {
        let Some(ty) = types.get(candidate.input) else { continue };
        if ty.is_copy(db) || !ty.is_sized(db) || ty.impls_fnonce(db) {
            continue;
        }
        if ty.impls_trait_fresh(db, range_bounds) {
            continue;
        }
        let bounds = ty.env_bounds(db);
        if borrow.is_some_and(|borrow| bounds.iter().any(|(it, _)| *it == borrow)) {
            continue;
        }
        if !bounds.is_empty() {
            let reference = ty.ref_to(db, hir::Mutability::Shared);
            if bounds.iter().all(|(it, args)| reference.impls_trait(db, *it, args)) {
                continue;
            }
        }
        let Some(local) = cx.local(candidate.pat) else { continue };
        if analysis.consumes(local) {
            continue;
        }
        found.push((candidate, ty.clone()));
    }
    for (candidate, ty) in found {
        report(cx, node, body, candidate, &ty);
    }
}

fn report<'q, 't>(
    cx: &mut Cx<'q, 't>,
    node: ast::Node<'t>,
    body: ast::BlockExpr<'t>,
    candidate: Candidate<'t>,
    ty: &hir::Type<'q>,
) {
    let db = cx.db();
    let Some(range) = cx.range(candidate.ty.syntax()) else { return };
    let span = cx.span(range);
    let adt = ty.as_adt();
    let copy_help = adt
        .filter(|_| ty.may_implement_copy(db))
        .and_then(|adt| cx.def_span(adt));
    // The changes of the type and of each `.clone()`, when the type is `Vec` or `String`.
    let mut retyped: Option<(String, Vec<(TextRange, String)>)> = None;
    if adt.is_some() && adt == cx.krate().diagnostic_item_adt(db, &intern::Symbol::intern("Vec")) {
        if let Some(element) = vec_element(candidate.ty)
            && let Some(local) = cx.local(candidate.pat)
            && let Some(clones) = clone_spans(cx, body.syntax(), local, &[("clone", ".to_owned()")])
        {
            let element = cx.range(element.syntax()).map_or("_", |it| cx.snippet(it));
            retyped = Some((format!("&[{element}]"), clones));
        }
    } else if adt.is_some() && adt == string_adt(cx)
        && let Some(local) = cx.local(candidate.pat)
        && let Some(clones) =
            clone_spans(cx, body.syntax(), local, &[("clone", ".to_string()"), ("as_str", "")])
    {
        retyped = Some(("&str".to_owned(), clones));
    }
    let retyped: Option<(String, Vec<(Span, String, String)>)> = retyped.map(|(code, clones)| {
        let clones = clones
            .into_iter()
            .map(|(at, code)| (cx.span(at), format!("change `{}` to", cx.snippet(at)), code))
            .collect();
        (code, clones)
    });
    let diagnostic = cx.lint(
        LINT,
        node,
        span.clone(),
        "this argument is passed by value, but not consumed in the function body",
    );
    if let Some(def_span) = copy_help {
        diagnostic.span_help(def_span, "or consider marking this type as `Copy`");
    }
    match retyped {
        Some((code, clones)) => {
            diagnostic.suggestion(
                span,
                "consider changing the type to",
                code,
                Applicability::Unspecified,
            );
            for (at, message, code) in clones {
                diagnostic.suggestion(at, message, code, Applicability::Unspecified);
            }
        }
        None => {
            diagnostic.suggestion_with_style(
                vec![(span.shrink_to_lo(), "&".to_owned())],
                "consider taking a reference instead",
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowAlways,
            );
        }
    }
}

/// Whether the type written `ty` can be one clippy lints: not a reference, a pointer or a `fn`
/// pointer (all `Copy`), nor a slice or `dyn` (unsized).
fn by_value(ty: ast::Type<'_>) -> bool {
    match ty {
        ast::Type::RefType(_)
        | ast::Type::PtrType(_)
        | ast::Type::FnPtrType(_)
        | ast::Type::NeverType(_)
        | ast::Type::SliceType(_)
        | ast::Type::DynTraitType(_) => false,
        ast::Type::ParenType(it) => it.ty().is_some_and(by_value),
        _ => true,
    }
}

/// clippy's `requires_exact_signature`: a proc macro's function.
fn requires_exact_signature(function: ast::Fn<'_>) -> bool {
    function.attrs().any(|attr| {
        matches!(attr.simple_name(), Some("proc_macro" | "proc_macro_attribute" | "proc_macro_derive"))
    })
}

/// The trait `#[rustc_diagnostic_item = name]` names, among the items the crate sees.
fn diagnostic_trait(cx: &Cx<'_, '_>, name: &str) -> Option<hir::Trait> {
    let items = hir_def::diagnostic_item::diagnostic_items(cx.db(), cx.krate().base());
    match items.get(&intern::Symbol::intern(name))? {
        hir_def::AttrDefId::TraitId(it) => Some(hir::Trait::from(it)),
        _ => None,
    }
}

/// `String`, the lang item.
fn string_adt(cx: &Cx<'_, '_>) -> Option<hir::Adt> {
    let items = hir_def::lang_item::lang_items(cx.db(), cx.krate().base());
    match LangItem::String.from_lang_items(items)? {
        hir_def::lang_item::LangItemTarget::StructId(it) => {
            Some(hir::Adt::from(hir_def::AdtId::StructId(it)))
        }
        _ => None,
    }
}

/// The element type written in `Vec<T>`: the first type argument of the path's segment named
/// `Vec`, for a path that is not relative to a type (`QPath::Resolved`).
fn vec_element(ty: ast::Type<'_>) -> Option<ast::Type<'_>> {
    let ast::Type::PathType(path) = ty else { return None };
    let path = path.path()?;
    let segment = path.segments().find(|it| it.name_ref().is_some_and(|it| it.text() == "Vec"))?;
    segment.generic_arg_list()?.generic_args().find_map(|it| match it {
        ast::GenericArg::TypeArg(it) => it.ty(),
        _ => None,
    })
}

/// clippy's `extract_clone_suggestions`: each call `local.name()` of the body, outside closures,
/// of a method `replacements` names, with what replaces it; `None` when the body calls
/// `local.capacity()`, which a slice or `str` does not have.
fn clone_spans<'q, 't>(
    cx: &Cx<'q, 't>,
    body: ast::Node<'t>,
    local: hir::Local<'q>,
    replacements: &[(&str, &'static str)],
) -> Option<Vec<(TextRange, String)>> {
    let mut found = Vec::new();
    let complete = calls_in(cx, body, local, replacements, &mut found, 0);
    complete.then_some(found)
}

/// Whether the walk of `node` went to its end: [`clone_spans`]'s visitor.
fn calls_in<'q, 't>(
    cx: &Cx<'q, 't>,
    node: ast::Node<'t>,
    local: hir::Local<'q>,
    replacements: &[(&str, &'static str)],
    found: &mut Vec<(TextRange, String)>,
    depth: u32,
) -> bool {
    if let Some(call) = ast::MacroCall::cast(node) {
        return depth >= DEEPEST
            || cx
                .expand(call)
                .is_none_or(|it| calls_in(cx, it, local, replacements, found, depth + 1));
    }
    if ast::ClosureExpr::can_cast(node.kind()) || ast::Item::can_cast(node.kind()) {
        return true;
    }
    if let Some(call) = ast::MethodCallExpr::cast(node)
        && call.arg_list().is_some_and(|it| it.args().next().is_none())
        && let Some(receiver) = call.receiver()
        && is_local(cx, receiver, local)
        && let Some(name) = call.name_ref()
    {
        if name.text() == "capacity" {
            return false;
        }
        if let Some((_, suffix)) = replacements.iter().find(|(it, _)| *it == name.text()) {
            if let Some(at) = cx.range(node) {
                found.push((at, format!("{}{suffix}", cx.source_of(receiver.syntax()))));
            }
            return true;
        }
    }
    node.children().all(|child| calls_in(cx, child, local, replacements, found, depth))
}

/// clippy's `path_to_local_id`: `expr` is a path naming `local`.
fn is_local<'q, 't>(cx: &Cx<'q, 't>, expr: ast::Expr<'t>, local: hir::Local<'q>) -> bool {
    let ast::Expr::PathExpr(path) = expr else { return false };
    matches!(path.path().and_then(|it| cx.path(it)), Some(PathResolution::Local(it)) if it == local)
}
