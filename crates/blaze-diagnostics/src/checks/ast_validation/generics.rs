//! The checks of rustc's AST validation of generics, bounds and generic arguments: defaults
//! and the order of generic parameters, nested `for<..>` (E0316), `const` and `[const]` bounds
//! where the context does not allow them, `use<..>` outside `impl Trait`, and generic arguments
//! after a constraint.
//!
//! Follows rustc's `compiler/rustc_ast_passes/src/ast_validation.rs` (`visit_generics`,
//! `validate_generic_param_order`, `visit_param_bound`, `visit_generic_args`,
//! `check_generic_args_before_constraints`, `correct_generic_order_suggestion`,
//! `with_tilde_const` and where each item, function, associated type and anonymous constant
//! sets what it allows), `diagnostics.rs` (`TildeConstReason`) and
//! `compiler/rustc_ast/src/visit.rs` (`BoundKind::descr`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{
    SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasName as _, HasTypeBounds as _},
};

use super::{Ctxt, Out, V, item_range, unraw};
use crate::{
    lints::syntax::pretty,
    model::{Applicability, Diag},
};

/// `visit_generics` of a generic parameter list: defaults that are not trailing, then
/// lifetimes after types or consts. A `for<..>` binder's parameters are not `Generics`.
pub(super) fn params<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    if node.parent().is_some_and(|it| it.kind() == SyntaxKind::FOR_BINDER) {
        return;
    }
    let Some(list) = ast::GenericParamList::cast(node) else { return };
    let mut prev_default = None;
    for param in list.generic_params().filter(|it| v.kept(it.syntax())) {
        let default = match param {
            ast::GenericParam::LifetimeParam(_) => continue,
            ast::GenericParam::TypeParam(it) => it.default_type().is_some(),
            ast::GenericParam::ConstParam(it) => it.default_val().is_some(),
        };
        if default {
            prev_default = param_ident(param);
        } else if let Some(range) = prev_default {
            let message = "generic parameters with a default must be trailing";
            v.emit(out, node, Diag::error(None, message, v.span(range)));
            break;
        }
    }
    order(v, node, list, out);
}

/// Where a generic parameter's name is written.
fn param_ident(param: ast::GenericParam<'_>) -> Option<TextRange> {
    match param {
        ast::GenericParam::LifetimeParam(it) => it.lifetime().map(|it| it.syntax().text_range()),
        ast::GenericParam::TypeParam(it) => it.name().map(|it| it.syntax().text_range()),
        ast::GenericParam::ConstParam(it) => it.name().map(|it| it.syntax().text_range()),
    }
}

/// `validate_generic_param_order`: lifetimes declared after a type or const parameter, with
/// the list rustc suggests instead, printed as its pretty printer prints it.
fn order<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, list: ast::GenericParamList<'db>, out: &mut Out) {
    let out_of_order = move || {
        let mut seen_type_or_const = false;
        list.generic_params().filter(move |it| v.kept(it.syntax())).filter_map(move |param| match param {
            ast::GenericParam::LifetimeParam(_) if seen_type_or_const => param_ident(param),
            ast::GenericParam::LifetimeParam(_) => None,
            _ => {
                seen_type_or_const = true;
                None
            }
        })
    };
    let Some(first) = out_of_order().next() else { return };
    let edition = v.edition_at(node.text_range().start());
    // alloc: the error's suggestion
    let mut ordered = String::from("<");
    let params = || list.generic_params().filter(|it| v.kept(it.syntax()));
    let lifetimes = params().filter(|it| matches!(it, ast::GenericParam::LifetimeParam(_)));
    let others = params().filter(|it| !matches!(it, ast::GenericParam::LifetimeParam(_)));
    for (i, param) in lifetimes.chain(others).enumerate() {
        if i != 0 {
            ordered.push_str(", ");
        }
        print_param(&mut ordered, param, edition);
    }
    ordered.push('>');
    let message = "lifetime parameters must be declared prior to type and const parameters";
    let mut diag = Diag::error(None, message, v.span(first));
    for range in out_of_order().skip(1) {
        diag.primary(v.span(range));
    }
    diag.suggestion(
        v.span(node.text_range()),
        "reorder the parameters: lifetimes, then consts and types",
        ordered,
        Applicability::MachineApplicable,
    );
    v.emit(out, node, diag);
}

/// How `validate_generic_param_order` prints a parameter: its name (`const N: ty` for a
/// const), its bounds, its default.
fn print_param(out: &mut String, param: ast::GenericParam<'_>, edition: span::Edition) {
    match param {
        ast::GenericParam::LifetimeParam(it) => {
            if let Some(lifetime) = it.lifetime() {
                out.push_str(lifetime.text());
            }
            bounds(out, it.type_bound_list(), edition);
        }
        ast::GenericParam::TypeParam(it) => {
            if let Some(name) = it.name() {
                pretty::push_ident(out, unraw(name.text()), edition);
            }
            bounds(out, it.type_bound_list(), edition);
            if let Some(default) = it.default_type() {
                out.push_str(" = ");
                pretty::ty(out, default, edition);
            }
        }
        ast::GenericParam::ConstParam(it) => {
            out.push_str("const ");
            if let Some(name) = it.name() {
                pretty::push_ident(out, unraw(name.text()), edition);
            }
            out.push_str(": ");
            if let Some(ty) = it.ty() {
                pretty::ty(out, ty, edition);
            }
            if let Some(expr) = it.default_val().and_then(|it| it.expr()) {
                out.push_str(" = ");
                pretty::expr(out, expr, edition);
            }
        }
    }
}

fn bounds(out: &mut String, list: Option<ast::TypeBoundList<'_>>, edition: span::Edition) {
    if let Some(list) = list.filter(|it| it.bounds().next().is_some()) {
        out.push_str(": ");
        pretty::bounds(out, list, edition);
    }
}

/// Whether a `for<..>` binder declares any parameter.
fn binds(binder: Option<ast::ForBinder<'_>>) -> bool {
    binder.and_then(|it| it.generic_param_list()).is_some_and(|it| it.generic_params().next().is_some())
}

/// `visit_generics` of a `where` predicate: E0316 for each trait bound with a `for<..>` of
/// its own under the predicate's.
pub(super) fn where_pred<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(pred) = ast::WherePred::cast(node) else { return };
    if pred.lifetime().is_some() || !binds(pred.for_binder()) {
        return;
    }
    let Some(list) = pred.type_bound_list() else { return };
    for bound in list.bounds().filter(|it| it.lifetime().is_none() && it.use_bound_generic_args().is_none()) {
        if binds(bound.for_binder()) {
            let span = v.span(bound.syntax().text_range());
            v.emit(out, node, Diag::error(Some("E0316"), "nested quantification of lifetimes", span));
        }
    }
}

/// `BoundKind`: where a list of bounds is.
#[derive(PartialEq)]
enum BoundKind {
    Bound,
    Impl,
    TraitObject,
    SuperTraits,
}

impl BoundKind {
    fn of(bound: ast::Node<'_>) -> BoundKind {
        let owner = bound.parent().and_then(|it| it.parent());
        match owner.map(|it| it.kind()) {
            Some(SyntaxKind::DYN_TRAIT_TYPE) => BoundKind::TraitObject,
            Some(SyntaxKind::IMPL_TRAIT_TYPE) => BoundKind::Impl,
            Some(SyntaxKind::TRAIT) => BoundKind::SuperTraits,
            _ => BoundKind::Bound,
        }
    }

    fn descr(&self) -> &'static str {
        match self {
            BoundKind::Bound => "bounds",
            BoundKind::Impl => "`impl Trait`",
            BoundKind::TraitObject => "`dyn` trait object bounds",
            BoundKind::SuperTraits => "supertrait bounds",
        }
    }
}

/// `visit_param_bound`: `const` in a trait object, `[const]` where the context does not
/// allow it, and `use<..>` outside `impl Trait`.
pub(super) fn bound<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(bound) = ast::TypeBound::cast(node) else { return };
    let kind = BoundKind::of(node);
    if bound.use_bound_generic_args().is_some() {
        if kind != BoundKind::Impl {
            // alloc: the error's message
            let message = format!("`use<...>` precise capturing syntax not allowed in {}", kind.descr());
            v.emit(out, node, Diag::error(None, message, v.span(node.text_range())));
        }
        return;
    }
    if bound.lifetime().is_some() || bound.question_mark_token().is_some() {
        return;
    }
    let Some(const_) = bound.const_token() else { return };
    let maybe = match (bound.l_brack_token(), bound.r_brack_token(), bound.tilde_token()) {
        (Some(l), Some(r), _) => Some(l.text_range().cover(r.text_range())),
        (_, _, Some(tilde)) => Some(tilde.text_range().cover(const_.text_range())),
        _ => None,
    };
    let Some(maybe) = maybe else {
        if kind == BoundKind::TraitObject {
            let message = "const trait bounds are not allowed in trait object types";
            v.emit(out, node, Diag::error(None, message, v.span(node.text_range())));
        }
        return;
    };
    let Some(reason) = tilde_const_reason(v, node) else { return };
    let mut diag = Diag::error(None, "`[const]` is not allowed here", v.span(maybe));
    match reason.1 {
        Some(range) => diag.span_note(v.cx.span(range), reason.0),
        None => diag.note(reason.0),
    };
    v.emit(out, node, diag);
}

/// `disallow_tilde_const` at `node`: why the innermost context that decides it does not allow
/// `[const]` (with the span rustc's note points at), or `None` where it is allowed.
fn tilde_const_reason<'db>(
    v: &V<'_, '_, 'db>,
    node: ast::Node<'db>,
) -> Option<(&'static str, Option<InFile<TextRange>>)> {
    let mut child = node;
    for at in v.ancestors(node) {
        let parent = at.value;
        let from = std::mem::replace(&mut child, parent);
        let file = at.file_id;
        let at_range = |range: TextRange| Some(InFile::new(file, range));
        let range = |node: ast::Node<'_>| at_range(item_range(node));
        match parent.kind() {
            SyntaxKind::DYN_TRAIT_TYPE => return Some(("trait objects cannot have `[const]` trait bounds", None)),
            SyntaxKind::CONST_ARG => {
                let expr = ast::ConstArg::cast(parent).and_then(|it| it.expr());
                let span = expr.map_or(parent.text_range(), |it| it.syntax().text_range());
                return Some(("anonymous constants cannot have `[const]` trait bounds", at_range(span)));
            }
            SyntaxKind::ARRAY_EXPR => {
                let count = ast::ArrayExpr::cast(parent)
                    .filter(|it| it.semicolon_token().is_some())
                    .and_then(|it| it.exprs().nth(1));
                if let Some(count) = count.filter(|it| it.syntax() == from) {
                    let span = at_range(count.syntax().text_range());
                    return Some(("anonymous constants cannot have `[const]` trait bounds", span));
                }
            }
            SyntaxKind::BLOCK_EXPR => {
                let block = ast::BlockExpr::cast(parent);
                if let Some(block) = block.filter(|it| it.const_token().is_some()) {
                    let span = block.stmt_list().map_or(parent.text_range(), |it| it.syntax().text_range());
                    return Some(("anonymous constants cannot have `[const]` trait bounds", at_range(span)));
                }
            }
            SyntaxKind::CLOSURE_EXPR => return Some(("closures cannot have `[const]` trait bounds", None)),
            SyntaxKind::FN => {
                let Some(item) = ast::Fn::cast(parent) else { continue };
                let ctxt = v.ctxt_in(at.file_id, parent);
                if item.const_token().is_some() || ctxt.is_assoc() && ctxt.constness().is_some() {
                    return None;
                }
                let name = item.name().and_then(|it| at_range(it.syntax().text_range()));
                return Some(("this function is not `const`, so it cannot have `[const]` trait bounds", name));
            }
            SyntaxKind::TYPE_ALIAS => {
                let ctxt = v.ctxt_in(at.file_id, parent);
                if !ctxt.is_assoc() {
                    continue;
                }
                if ctxt.constness().is_some() {
                    return None;
                }
                let note = match ctxt {
                    Ctxt::Trait(_) => "associated types in non-`const` traits cannot have `[const]` trait bounds",
                    Ctxt::TraitImpl(_) => "associated types in non-const impls cannot have `[const]` trait bounds",
                    _ => "inherent associated types cannot have `[const]` trait bounds",
                };
                return Some((note, range(parent)));
            }
            SyntaxKind::TRAIT | SyntaxKind::IMPL => {
                let in_generics = matches!(
                    from.kind(),
                    SyntaxKind::GENERIC_PARAM_LIST | SyntaxKind::WHERE_CLAUSE | SyntaxKind::TYPE_BOUND_LIST
                );
                if !in_generics {
                    continue;
                }
                if super::const_token(parent).is_some() {
                    return None;
                }
                let note = match ast::Impl::cast(parent) {
                    None => "this trait is not `const`, so it cannot have `[const]` trait bounds",
                    Some(it) if it.trait_().is_some() => {
                        "this impl is not `const`, so it cannot have `[const]` trait bounds"
                    }
                    Some(_) => "inherent impls cannot have `[const]` trait bounds",
                };
                return Some((note, range(parent)));
            }
            SyntaxKind::STRUCT => return Some(("structs cannot have `[const]` trait bounds", range(parent))),
            SyntaxKind::ENUM => return Some(("enums cannot have `[const]` trait bounds", range(parent))),
            SyntaxKind::UNION => return Some(("unions cannot have `[const]` trait bounds", range(parent))),
            _ => {}
        }
    }
    Some(("this item cannot have `[const]` trait bounds", None))
}

/// `check_generic_args_before_constraints`: generic arguments after a constraint, with the
/// order rustc suggests (lifetimes, the other arguments, the constraints).
pub(super) fn args<'db>(v: &V<'_, '_, 'db>, node: ast::Node<'db>, out: &mut Out) {
    let Some(list) = ast::GenericArgList::cast(node) else { return };
    let is_constraint = |it: &ast::GenericArg<'db>| matches!(it, ast::GenericArg::AssocTypeArg(_));
    let mut seen_constraint = false;
    let partitioned = list.generic_args().all(|it| {
        let constraint = is_constraint(&it);
        let ok = constraint || !seen_constraint;
        seen_constraint |= constraint;
        ok
    });
    if partitioned {
        return;
    }
    let (constraints, generic): (Vec<ast::GenericArg<'db>>, Vec<ast::GenericArg<'db>>) =
        list.generic_args().partition(is_constraint);
    // alloc: on the error path (arguments after a constraint)
    let constraint_spans: Vec<_> = constraints.iter().map(|it| v.span(it.syntax().text_range())).collect();
    // alloc: as above
    let arg_spans: Vec<_> = generic.iter().map(|it| v.span(it.syntax().text_range())).collect();
    let (one_constraint, one_arg) = (constraints.len() == 1, generic.len() == 1);
    let edition = v.edition_at(node.text_range().start());
    // alloc: the error's suggestion
    let mut suggestion = String::from("<");
    let lifetimes = generic.iter().filter(|it| matches!(it, ast::GenericArg::LifetimeArg(_)));
    let others = generic.iter().filter(|it| !matches!(it, ast::GenericArg::LifetimeArg(_)));
    for (i, arg) in lifetimes.chain(others).chain(&constraints).enumerate() {
        if i != 0 {
            suggestion.push_str(", ");
        }
        pretty::generic_arg(&mut suggestion, *arg, edition);
    }
    suggestion.push('>');
    let data = match (list.l_angle_token(), list.r_angle_token()) {
        (Some(l), Some(r)) => l.text_range().cover(r.text_range()),
        _ => node.text_range(),
    };
    let message = "generic arguments must come before the first constraint";
    // clone: the error's span
    let mut diag = Diag::error(None, message, arg_spans[0].clone());
    for span in &arg_spans[1..] {
        // clone: the error's span
        diag.primary(span.clone());
    }
    let constraint_word = if one_constraint { "constraint" } else { "constraints" };
    let arg_word = if one_arg { "argument" } else { "arguments" };
    // clone: the error's span
    diag.span_label(constraint_spans[0].clone(), constraint_word);
    // alloc: clone: the error's label and span
    diag.span_label(arg_spans[arg_spans.len() - 1].clone(), format!("generic {arg_word}"));
    diag.suggestion_verbose(
        v.span(data),
        // alloc: the error's suggestion
        format!("move the {constraint_word} after the generic {arg_word}"),
        suggestion,
        Applicability::MachineApplicable,
    );
    for span in constraint_spans.into_iter().chain(arg_spans) {
        diag.span_label(span, "");
    }
    v.emit(out, node, diag);
}
