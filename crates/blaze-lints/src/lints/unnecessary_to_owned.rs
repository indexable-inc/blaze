//! `unnecessary_to_owned`: `to_owned`, `to_string`, `to_vec`, `to_path_buf`, `to_os_string` or
//! `Cow::into_owned` whose owned value is only borrowed again, where the receiver serves.
//!
//! Follows clippy's `clippy_lints/src/methods/unnecessary_to_owned.rs` (MIT or Apache-2.0),
//! written from what is known of its source rather than read from a checkout: no copy of
//! clippy's source was at hand. Its name, messages, spans, suggestions and applicabilities are
//! clippy's, for these of its cases, asked in clippy's order:
//! - `x.to_string().split(..)` (`check_split_call_arg`), for the receiver of `split`;
//! - `&x.to_owned()` coerced to what `x` already gives (`check_addr_of_expr`);
//! - `x.to_vec().into_iter()` and `IntoIterator::into_iter(x.to_vec())`
//!   (`check_into_iter_call_arg`), but not as a `for` loop's iterable, where clippy picks the
//!   suggestion by what the loop's body does with each item (`unnecessary_iter_cloned`);
//! - `&String::from_utf8(x.to_vec()).unwrap()` as a `&str` (`check_string_from_utf8`);
//! - `f(x.to_string())` where `f`'s parameter is a type parameter bounded by `AsRef` or `Deref`
//!   alone (`check_other_call_arg`), when the parameter's type is the type parameter itself and
//!   no other parameter, nor the return type, names it.
//!
//! Not ported, so never flagged: `cloned` and `copied` of iterators (`unnecessary_iter_cloned`),
//! `for` loops over the owned value, and the `Borrow`-keyed lookups of maps and sets
//! (`check_borrow_predicate`).

use hir::{Adjust, AsAssocItem as _, Mutability, PathResolution};
use syntax::{TextRange, view as ast};

use ast::{AstNode as _, HasArgList as _};

use super::{
    implicit_clone::{
        AS_REF, COW, DEREF, STRING, TO_STRING, inherent_of, is_clone_like, is_std_adt, of_trait,
        peel_refs,
    },
    utils::{self, MethodCall},
};
use crate::{check::Cx, diag::Applicability, msrvs};

const LINT: super::Implemented = super::Implemented::UnnecessaryToOwned;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    let Some(method) = ast::MethodCallExpr::cast(node) else { return };
    let expr = ast::Expr::MethodCallExpr(method);
    let Some(call) = utils::method_call(cx, expr) else { return };
    let owning = matches!(
        call.name,
        "to_owned" | "to_string" | "to_vec" | "to_path_buf" | "to_os_string" | "into_owned"
    );
    if !owning || !call.args().is_empty() {
        return;
    }
    let Some(function) = cx.method(method) else { return };
    if !is_to_owned_like(cx, call, function) {
        return;
    }
    if check_split_call_arg(cx, node, expr, call)
        || check_addr_of_expr(cx, node, expr, call, function)
        || check_into_iter_call_arg(cx, node, expr, call)
        || check_string_from_utf8(cx, node, expr, call)
    {
        return;
    }
    check_other_call_arg(cx, node, expr, call);
}

/// clippy's `is_to_owned_like`.
fn is_to_owned_like(cx: &Cx<'_, '_>, call: MethodCall<'_>, function: hir::Function) -> bool {
    is_cow_into_owned(cx, call.name, function)
        || (call.name != "to_string" && is_clone_like(cx, call.name, function))
        || is_to_string_on_string_like(cx, call, function)
}

/// clippy's `is_cow_into_owned`.
fn is_cow_into_owned(cx: &Cx<'_, '_>, name: &str, function: hir::Function) -> bool {
    name == "into_owned" && inherent_of(cx, function, COW)
}

/// clippy's `is_to_string_on_string_like`: `ToString::to_string` of a `Self` that derefs to
/// `str` or is `AsRef<str>`.
fn is_to_string_on_string_like(
    cx: &Cx<'_, '_>,
    call: MethodCall<'_>,
    function: hir::Function,
) -> bool {
    if call.name != "to_string" || !of_trait(cx, function, TO_STRING) {
        return false;
    }
    // `to_string(&self)`: `Self` is what the adjusted receiver points to.
    let Some((self_ty, _)) = cx.adjusted_ty(call.receiver).and_then(|it| it.as_reference()) else {
        return false;
    };
    let db = cx.db();
    let derefs_to_str = cx
        .std_trait(DEREF)
        .and_then(|deref| cx.assoc_ty(&self_ty, deref, "Target"))
        .is_some_and(|it| it.is_str());
    derefs_to_str
        || cx.std_trait(AS_REF).is_some_and(|as_ref| {
            self_ty.impls_trait(db, as_ref, &[hir::BuiltinType::str().ty(db)])
        })
}

/// clippy's `check_split_call_arg`: `x.to_string().split(p)`, which `x.split(p)` says.
fn check_split_call_arg<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    expr: ast::Expr<'t>,
    call: MethodCall<'t>,
) -> bool {
    let Some((parent @ ast::Expr::MethodCallExpr(_), child)) = parent_expr(expr.syntax()) else {
        return false;
    };
    let Some(split) = utils::method_call(cx, parent) else { return false };
    let [arg] = split.args() else { return false };
    if split.name != "split" || split.receiver.syntax() != child {
        return false;
    }
    let Some(range) = cx.range(parent.syntax()) else { return false };
    // `x` is `AsRef<str>` but not `Deref<Target = str>`: `split` is reached through `as_ref`.
    let as_ref = cx.ty(expr).is_some_and(|it| is_std_adt(cx, &it, STRING))
        && cx.ty(call.receiver).is_some_and(|receiver| {
            cx.std_trait(DEREF)
                .and_then(|deref| cx.assoc_ty(&receiver, deref, "Target"))
                .is_none_or(|it| !it.is_str())
        });
    let as_ref = if as_ref { ".as_ref()" } else { "" };
    let sugg = format!(
        "{}{as_ref}.split({})",
        cx.source_of(call.receiver.syntax()),
        cx.source_of(arg.syntax())
    );
    // Removing the owned value can leave the iterator borrowing what is later used mutably
    // (rust-clippy#8148).
    lint(cx, node, range, call.name, "use", sugg, Applicability::MaybeIncorrect);
    true
}

/// clippy's `check_addr_of_expr`: `&x.to_owned()` where the reference is coerced to what `x`
/// gives as it is.
fn check_addr_of_expr<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    expr: ast::Expr<'t>,
    call: MethodCall<'t>,
    function: hir::Function,
) -> bool {
    let Some((ast::Expr::RefExpr(reference), _)) = parent_expr(expr.syntax()) else {
        return false;
    };
    if reference.mut_token().is_some() || reference.raw_token().is_some() {
        return false;
    }
    let Some(adjustments) = cx.adjustments(ast::Expr::RefExpr(reference)) else { return false };
    let (referent_ty, target_ty) = match adjustments.as_slice() {
        // `Cow::from(&x.to_owned())`
        [
            hir::Adjustment { kind: Adjust::Deref(None), target: referent, .. },
            hir::Adjustment { kind: Adjust::Borrow(_), target, .. },
        ]
        // arrays
        | [
            hir::Adjustment { kind: Adjust::Deref(None), target: referent, .. },
            hir::Adjustment { kind: Adjust::Borrow(_), .. },
            hir::Adjustment { kind: Adjust::Pointer(_), target, .. },
        ]
        // everything else
        | [
            hir::Adjustment { kind: Adjust::Deref(None), target: referent, .. },
            hir::Adjustment { kind: Adjust::Deref(Some(_)), .. },
            hir::Adjustment { kind: Adjust::Borrow(_), target, .. },
        ] => (referent.clone(), target.clone()),
        _ => return false,
    };
    let Some(receiver_ty) = cx.ty(call.receiver) else { return false };
    let (target_ty, n_target_refs) = peel_refs(target_ty);
    let (receiver_ty, n_receiver_refs) = peel_refs(receiver_ty);
    let db = cx.db();
    let cow_into_owned = is_cow_into_owned(cx, call.name, function);
    // Distinct from `redundant_clone`'s cases (and rust-clippy#8759): the referent is not the
    // receiver's type, or is a `Copy` array, or the method is `Cow::into_owned`.
    if referent_ty == receiver_ty
        && !(referent_ty.is_array() && referent_ty.is_copy(db))
        && !cow_into_owned
    {
        return false;
    }
    let Some(range) = cx.range(reference.syntax()) else { return false };
    let receiver_snippet = cx.source_of(call.receiver.syntax());
    if receiver_ty == target_ty && n_target_refs >= n_receiver_refs {
        let sugg = format!("{}{receiver_snippet}", "&".repeat(n_target_refs - n_receiver_refs));
        lint(cx, node, range, call.name, "use", sugg, Applicability::MachineApplicable);
        return true;
    }
    let app = Applicability::MachineApplicable;
    if let Some(deref) = cx.std_trait(DEREF)
        && receiver_ty.impls_trait(db, deref, &[])
        && cx.assoc_ty(&receiver_ty, deref, "Target").is_some_and(|it| it == target_ty)
        // The `to_string` called is the one of what the receiver derefs to (rust-clippy#10033).
        && (cx.adjusted_ty(call.receiver).is_some_and(|it| it.strip_references() == target_ty)
            || cow_into_owned)
    {
        if n_receiver_refs > 0 {
            lint(cx, node, range, call.name, "use", receiver_snippet.into_owned(), app);
        } else {
            let (Some(receiver), Some(whole)) =
                (cx.range(call.receiver.syntax()), cx.range(expr.syntax()))
            else {
                return false;
            };
            let call_range = TextRange::new(receiver.end(), whole.end());
            lint(cx, node, call_range, call.name, "remove this", String::new(), app);
        }
        return true;
    }
    if let Some(as_ref) = cx.std_trait(AS_REF)
        && receiver_ty.impls_trait(db, as_ref, &[target_ty])
    {
        let sugg = format!("{receiver_snippet}.as_ref()");
        lint(cx, node, range, call.name, "use", sugg, Applicability::MachineApplicable);
        return true;
    }
    false
}

/// clippy's `check_into_iter_call_arg`: the owned value only turned into an iterator, which
/// iterating the receiver and cloning each item does.
fn check_into_iter_call_arg<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    expr: ast::Expr<'t>,
    call: MethodCall<'t>,
) -> bool {
    let Some((parent, child)) = parent_expr(expr.syntax()) else { return false };
    let callee = match parent {
        ast::Expr::MethodCallExpr(it) if it.receiver().is_some_and(|it| it.syntax() == child) => {
            cx.method(it)
        }
        ast::Expr::CallExpr(it) if arg_index(it.arg_list(), child).is_some() => callee_fn(cx, it),
        _ => None,
    };
    let Some(callee) = callee else { return false };
    let db = cx.db();
    let famous = cx.famous();
    let into_iterator = famous.core_iter_IntoIterator();
    let is_into_iter = callee.name(db).as_str() == "into_iter"
        && into_iterator.is_some()
        && callee.as_assoc_item(db).and_then(|it| it.container_or_implemented_trait(db))
            == into_iterator;
    // A `for` loop's iterable is `unnecessary_iter_cloned`'s, not ported.
    if !is_into_iter || is_for_iterable(parent.syntax()) {
        return false;
    }
    let Some(iterator) = famous.core_iter_Iterator() else { return false };
    let Some(parent_ty) = cx.ty(parent) else { return false };
    if !parent_ty.impls_trait(db, iterator, &[]) {
        return false;
    }
    let Some(item_ty) = cx.assoc_ty(&parent_ty, iterator, "Item") else { return false };
    // A `Cow`'s `into_owned` can't be removed in general (rust-clippy#13624); `iter()` of a
    // temporary is (rust-clippy#14242).
    let cow = cx.ty(call.receiver).is_none_or(|it| is_std_adt(cx, &it, COW));
    if cow || !is_place(cx, call.receiver) {
        return false;
    }
    let Some(range) = cx.range(parent.syntax()) else { return false };
    let cloned_or_copied = if item_ty.is_copy(db) && cx.conf().meets(msrvs::ITERATOR_COPIED) {
        "copied"
    } else {
        "cloned"
    };
    let sugg = format!("{}.iter().{cloned_or_copied}()", cx.source_of(call.receiver.syntax()));
    // The iterator left can borrow what is later used mutably (rust-clippy#8148).
    lint(cx, node, range, call.name, "use", sugg, Applicability::MaybeIncorrect);
    true
}

/// clippy's `check_string_from_utf8`: `&String::from_utf8(x.to_vec()).unwrap()` used as a
/// `&str`, which `core::str::from_utf8(&x).unwrap()` makes without allocating.
fn check_string_from_utf8<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    expr: ast::Expr<'t>,
    call: MethodCall<'t>,
) -> bool {
    let Some((ast::Expr::CallExpr(from_utf8), arg)) = skip_addr_of_ancestors(expr.syntax()) else {
        return false;
    };
    let db = cx.db();
    let (Some(callee), Some(function)) = (from_utf8.expr(), callee_fn(cx, from_utf8)) else {
        return false;
    };
    if arg_index(from_utf8.arg_list(), arg).is_none()
        || function.name(db).as_str() != "from_utf8"
        || !inherent_of(cx, function, STRING)
    {
        return false;
    }
    let Some((ast::Expr::MethodCallExpr(unwrap), child)) = parent_expr(from_utf8.syntax()) else {
        return false;
    };
    let unwraps = unwrap.receiver().is_some_and(|it| it.syntax() == child)
        && unwrap.name_ref().is_some_and(|it| matches!(it.text(), "unwrap" | "expect"));
    if !unwraps {
        return false;
    }
    let Some((ast::Expr::RefExpr(ref_string), _)) = parent_expr(unwrap.syntax()) else {
        return false;
    };
    // `&String::from_utf8(..).unwrap()` is a `&String`: only one coerced to `&str` is linted.
    let to_str = ref_string.mut_token().is_none()
        && ref_string.raw_token().is_none()
        && cx
            .adjusted_ty(ast::Expr::RefExpr(ref_string))
            .and_then(|it| it.as_reference())
            .is_some_and(|(inner, _)| inner.is_str());
    if !to_str {
        return false;
    }
    let (Some(unwrap_range), Some(callee_range), Some(ref_range), Some(arg_range)) = (
        cx.range(unwrap.syntax()),
        cx.range(callee.syntax()),
        cx.range(ref_string.syntax()),
        cx.range(arg),
    ) else {
        return false;
    };
    let borrow = if cx.ty(call.receiver).is_some_and(|it| it.is_reference()) { "" } else { "&" };
    let arg_sugg = format!("{borrow}{}", cx.source_of(call.receiver.syntax()));
    let parts = vec![
        (cx.span(callee_range), "core::str::from_utf8".to_owned()),
        (cx.span(TextRange::new(ref_range.start(), unwrap_range.start())), String::new()),
        (cx.span(arg_range), arg_sugg),
    ];
    let span = cx.span(unwrap_range);
    cx.lint(
        LINT,
        node,
        span,
        "allocating a new `String` only to create a temporary `&str` from it",
    )
    .multipart_suggestion(
        "convert from `&[u8]` to `&str` directly",
        parts,
        Applicability::MachineApplicable,
    );
    true
}

/// clippy's `check_other_call_arg`: the owned value handed to a parameter of a type parameter
/// whose one bound, `AsRef` or `Deref`, the receiver meets as well.
fn check_other_call_arg<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    expr: ast::Expr<'t>,
    call: MethodCall<'t>,
) -> bool {
    let Some((maybe_call, maybe_arg)) = skip_addr_of_ancestors(expr.syntax()) else {
        return false;
    };
    // The callee, and which of its inputs, the receiver first, `maybe_arg` is.
    let (function, index) = match maybe_call {
        ast::Expr::MethodCallExpr(it) => {
            let Some(function) = cx.method(it) else { return false };
            let index = if it.receiver().is_some_and(|it| it.syntax() == maybe_arg) {
                Some(0)
            } else {
                arg_index(it.arg_list(), maybe_arg).map(|it| it + 1)
            };
            (function, index)
        }
        ast::Expr::CallExpr(it) => {
            let Some(function) = callee_fn(cx, it) else { return false };
            (function, arg_index(it.arg_list(), maybe_arg))
        }
        _ => return false,
    };
    let Some(index) = index else { return false };
    let db = cx.db();
    let params = function.assoc_fn_params(db);
    let Some(param) = params.get(index) else { return false };
    // `can_change_type`: the input is the type parameter itself.
    let Some(type_param) = param.ty().as_type_param(db) else { return false };
    let sized = [cx.lang_trait(hir::LangItem::Sized), cx.lang_trait(hir::LangItem::MetaSized)];
    let bounds = type_param.trait_refs(db);
    let mut traits = bounds.iter().map(|it| it.trait_()).filter(|it| !sized.contains(&Some(*it)));
    let (Some(trait_), None) = (traits.next(), traits.next()) else { return false };
    let deref = cx.std_trait(DEREF);
    if Some(trait_) != deref && Some(trait_) != cx.std_trait(AS_REF) {
        return false;
    }
    let Some(receiver_ty) = cx.ty(call.receiver) else { return false };
    // `Target = &T` would not match `Target = T`: no `&` is added for `Deref`.
    let (n_refs, receiver_ty) = if receiver_ty.is_copy(db) {
        (0, receiver_ty)
    } else if Some(trait_) != deref {
        // `&receiver_ty`, as the receiver's autoref made it for the owning call.
        let Some(adjusted) = cx.adjusted_ty(call.receiver) else { return false };
        match adjusted.as_reference() {
            Some((inner, Mutability::Shared)) if inner == receiver_ty => (1, adjusted),
            _ => return false,
        }
    } else {
        return false;
    };
    let generic = hir::GenericParam::TypeParam(type_param);
    let elsewhere = params
        .iter()
        .enumerate()
        .any(|(at, it)| at != index && it.ty().generic_params(db).contains(&generic))
        || function.ret_type(db).generic_params(db).contains(&generic);
    if elsewhere {
        return false;
    }
    // Every bound of the parameter holds of the receiver's type as well.
    let mut args = Vec::new();
    for bound in &bounds {
        if sized.contains(&Some(bound.trait_())) {
            continue;
        }
        args.clear();
        for at in 1.. {
            let Some(arg) = bound.get_type_argument(at) else { break };
            let Some(arg) = arg.without_params(db) else { return false };
            args.push(arg);
        }
        if !receiver_ty.impls_trait(db, bound.trait_(), &args) {
            return false;
        }
    }
    let Some(range) = cx.range(maybe_arg) else { return false };
    let sugg = format!("{}{}", "&".repeat(n_refs), cx.source_of(call.receiver.syntax()));
    lint(cx, node, range, call.name, "use", sugg, Applicability::MachineApplicable);
    true
}

/// Reports at `range` with clippy's message for `method` and its one suggestion.
fn lint<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    range: TextRange,
    method: &str,
    help: &str,
    sugg: String,
    applicability: Applicability,
) {
    let span = cx.span(range);
    cx.lint(LINT, node, span.clone(), format!("unnecessary use of `{method}`"))
        .suggestion(span, help, sugg, applicability);
}

/// clippy's `get_parent_expr`: the expression `node` is in, parentheses passed (HIR has none),
/// and the node in it that holds `node`: `node` or its outermost parentheses. An argument's is
/// its call.
fn parent_expr(node: ast::Node<'_>) -> Option<(ast::Expr<'_>, ast::Node<'_>)> {
    let mut child = node;
    loop {
        let parent = child.parent()?;
        if ast::ParenExpr::can_cast(parent.kind()) {
            child = parent;
            continue;
        }
        if ast::ArgList::can_cast(parent.kind()) {
            return Some((ast::Expr::cast(parent.parent()?)?, child));
        }
        return Some((ast::Expr::cast(parent)?, child));
    }
}

/// clippy's `skip_addr_of_ancestors`: the expression around `node` past the shared borrows of
/// it, and the outermost of them (or `node`).
fn skip_addr_of_ancestors(node: ast::Node<'_>) -> Option<(ast::Expr<'_>, ast::Node<'_>)> {
    let mut at = node;
    loop {
        let (parent, child) = parent_expr(at)?;
        match parent {
            ast::Expr::RefExpr(it) if it.mut_token().is_none() && it.raw_token().is_none() => {
                at = it.syntax();
            }
            _ => return Some((parent, child)),
        }
    }
}

/// Which argument of `list` `node` is.
fn arg_index(list: Option<ast::ArgList<'_>>, node: ast::Node<'_>) -> Option<usize> {
    list?.args().position(|it| it.syntax() == node)
}

/// The function `call` calls by path, as clippy's `fn_def_id` finds it.
fn callee_fn(cx: &Cx<'_, '_>, call: ast::CallExpr<'_>) -> Option<hir::Function> {
    let ast::Expr::PathExpr(path) = call.expr()? else { return None };
    match cx.path(path.path()?)? {
        PathResolution::Def(hir::ModuleDef::Function(function)) => Some(function),
        _ => None,
    }
}

/// Whether `node`, its parentheses passed, is a `for` loop's iterable.
fn is_for_iterable(node: ast::Node<'_>) -> bool {
    let mut at = node;
    while let Some(parent) = at.parent().filter(|it| ast::ParenExpr::can_cast(it.kind())) {
        at = parent;
    }
    at.parent()
        .and_then(ast::ForExpr::cast)
        .and_then(|it| it.iterable())
        .is_some_and(|it| it.syntax() == at)
}

/// Whether `expr` is a place that outlives its statement: a local, a static, or a field of one.
/// Anything else is taken for a temporary, as clippy's `is_expr_temporary_value` at its widest.
fn is_place<'t>(cx: &Cx<'_, 't>, expr: ast::Expr<'t>) -> bool {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|it| is_place(cx, it)),
        ast::Expr::FieldExpr(it) => it.expr().is_some_and(|it| is_place(cx, it)),
        ast::Expr::PathExpr(it) => matches!(
            it.path().and_then(|it| cx.path(it)),
            Some(PathResolution::Local(_) | PathResolution::Def(hir::ModuleDef::Static(_)))
        ),
        _ => false,
    }
}
