//! E0023 "this pattern has N fields, but the corresponding tuple struct has M fields": a tuple
//! struct or tuple variant pattern (or the call it is written as on the left of a destructuring
//! assignment) with too many fields, or too few and no `..`; with rustc's labels on the
//! pattern and the definition, and its suggestions of the missing parentheses, `_` for each
//! missing field or `..` for them.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_pat_tuple_struct`,
//! `emit_err_pat_wrong_number_of_fields`), `compiler/rustc_ast_lowering/src/expr.rs`
//! (`destructure_assign_mut`, `destructure_sequence`) for the destructuring-assignment form, and
//! `compiler/rustc_middle/src/hir/map.rs` (`opt_ident`, `hir_span`, `named_span`) for the spans
//! of the definition (MIT or Apache-2.0). The struct or variant is the one inference checked the
//! pattern against; the missing-parentheses suggestion reads the field's type with the pattern's
//! type arguments, and is held back when the pattern's type did not match the scrutinee's.

use blaze_items::{Fact, ItemId};
use either::Either;
use hir::InFile;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasArgList as _, HasGenericParams as _, HasName as _, Node, RangeItem as _},
};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MismatchedTupleStructPatArgCount) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Some(pat) = pat(cx, d) else { return Vec::new() };
    let Some(Def { descr, span: def_span, ident, mut field_spans, fields: field_ids }) =
        def(cx, pat.variant)
    else {
        // alloc: empty, allocates nothing
        return Vec::new();
    };
    let subpats = pat.subpats.as_slice();
    let (found, fields) = (subpats.len(), field_ids.len());
    let parentheses = !d.had_err && missing_parentheses(cx, &pat, &field_ids);

    let first_subpat = subpats.first().map_or(&pat.span, |it| &it.span);
    let last_subpat = subpats.last().map_or(&pat.span, |it| &it.span);
    let mut diag = Diag::error(
        Some("E0023"),
        // alloc: the error's text, for an error hir found
        format!(
            "this pattern has {found} field{}, but the corresponding {descr} has {fields} field{}",
            s(found),
            s(fields),
        ),
        // clone: on the error path, for an error hir found
        first_subpat.clone(),
    );
    for it in subpats.iter().skip(1) {
        // clone: on the error path, for an error hir found
        diag.primary(it.span.clone());
    }
    // alloc: clone: the error's parts, for an error hir found
    diag.span_label(last_subpat.clone(), format!("expected {fields} field{}, found {found}", s(fields)));
    if multiline(cx, &pat.path, last_subpat) {
        // clone: on the error path, for an error hir found
        diag.span_label(pat.path.clone(), "");
    }
    let last_field = field_spans.pop().unwrap_or(def_span);
    if multiline(cx, &ident, &last_field) {
        // alloc: the error's text, for an error hir found
        diag.span_label(ident, format!("{descr} defined here"));
    }
    for span in field_spans {
        diag.span_label(span, "");
    }
    // alloc: the error's text, for an error hir found
    diag.span_label(last_field, format!("{descr} has {fields} field{}", s(fields)));

    if parentheses {
        let (left, right) = match (subpats.first(), subpats.last()) {
            (Some(first), Some(last)) => (first.span.shrink_to_lo(), last.span.shrink_to_hi()),
            _ => (pat.path.shrink_to_hi(), pat.span.shrink_to_hi()),
        };
        diag.multipart_suggestion(
            "missing parentheses",
            // alloc: the error's parts, for an error hir found
            vec![(left, "(".to_owned()), (right, ")".to_owned())],
            Applicability::MachineApplicable,
        );
    } else if fields > found && pat.span.hi > pat.span.lo {
        let at = pat.span.hi - 1;
        let after_fields = pat.span.with_lo(at).with_hi(at);
        let all_wildcards = subpats.iter().all(|it| it.wild);
        let missing = fields - found;
        let rest = (missing > 1 || all_wildcards).then(|| {
            if found == 0 || all_wildcards {
                let all_fields = match subpats {
                    [] => after_fields.clone(), // clone: on the error path, for an error hir found
                    [field] => field.span.clone(), // clone: on the error path, for an error hir found
                    [first, .., last] => first.span.to(&last.span),
                };
                return (all_fields, "use `..` to ignore all fields", "..");
            }
            let first_tail_wildcard =
                subpats.iter().enumerate().fold(None, |acc, (pos, it)| match (acc, it.wild) {
                    (None, true) => Some(pos),
                    (Some(_), true) => acc,
                    (_, false) => None,
                });
            let tail = match first_tail_wildcard {
                None => after_fields.clone(), // clone: on the error path, for an error hir found
                Some(0) => subpats[0].span.to(&after_fields),
                Some(pos) => subpats[pos - 1].span.shrink_to_hi().to(&after_fields),
            };
            (tail, "use `..` to ignore the rest of the fields", ", ..")
        });
        diag.suggestion_verbose(
            after_fields,
            "use `_` to explicitly ignore each field",
            wildcards(found, missing),
            Applicability::MaybeIncorrect,
        );
        if let Some((span, message, code)) = rest {
            diag.suggestion_verbose(span, message, code, Applicability::MaybeIncorrect);
        }
    }

    let adt = pat.variant.adt(cx.db);
    let name = pat.variant.name(cx.db);
    diag.fact(
        Fact::Signature(cx.key(ItemId::Adt(adt.into()))),
        // alloc: the error's text, for an error hir found
        format!("the fields of `{}`", name.display(cx.db, cx.edition)),
    );
    code_fact(cx, &mut diag, d.expr_or_pat.map(|it| it.syntax_node_ptr()));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `_` for each of the `missing` fields, after the `found` ones: `", _, _"`.
fn wildcards(found: usize, missing: usize) -> String {
    let mut code = String::with_capacity(missing * 3);
    for i in 0..missing {
        if i > 0 || found > 0 {
            code.push_str(", ");
        }
        code.push('_');
    }
    code
}

/// A field pattern, `..` left out: rustc's `subpats`.
struct Subpat {
    /// Without the parentheses around it, as rustc lowers them away.
    span: Span,
    /// `_`.
    wild: bool,
}

struct Pat<'db> {
    span: Span,
    /// The path, rustc's `qpath`.
    path: Span,
    subpats: Vec<Subpat>,
    variant: hir::Variant,
    /// The pattern's type.
    ty: Option<hir::Type<'db>>,
}

fn pat<'db>(cx: &Cx<'_, 'db>, d: &hir::MismatchedTupleStructPatArgCount) -> Option<Pat<'db>> {
    let node = cx.node_view(d.expr_or_pat);
    let file = node.file_id;
    let span_of = |it: Node<'_>| cx.span(InFile::new(file, it.text_range()));
    let (path, subpats, ty) = match node.value {
        Either::Right(ast::Pat::TupleStructPat(it)) => {
            let subpats = it
                .fields()
                .filter(|it| !matches!(it, ast::Pat::RestPat(_)))
                .map(|it| {
                    let it = strip_pat(it);
                    Subpat { span: span_of(it.syntax()), wild: matches!(it, ast::Pat::WildcardPat(_)) }
                })
                // alloc: the error's parts, for an error hir found
                .collect();
            let ty = cx.vsema.type_of_pat(ast::Pat::TupleStructPat(it));
            (it.path()?, subpats, ty)
        }
        Either::Left(ast::Expr::CallExpr(it)) => {
            let ast::Expr::PathExpr(callee) = it.expr()? else { return None };
            let subpats = it
                .arg_list()?
                .args()
                .filter(|it| !is_rest(*it))
                .map(|it| {
                    let it = strip_expr(it);
                    Subpat {
                        span: span_of(it.syntax()),
                        wild: matches!(it, ast::Expr::UnderscoreExpr(_)),
                    }
                })
                // alloc: the error's parts, for an error hir found
                .collect();
            let ty = cx.vsema.type_of_expr(ast::Expr::CallExpr(it));
            (callee.path()?, subpats, ty)
        }
        _ => return None,
    };
    Some(Pat {
        span: span_of(node.value.syntax()),
        path: span_of(path.syntax()),
        subpats,
        variant: d.variant,
        ty: ty.map(|it| it.original),
    })
}

fn strip_pat(pat: ast::Pat<'_>) -> ast::Pat<'_> {
    let inner = match pat {
        ast::Pat::ParenPat(it) => it.pat(),
        _ => None,
    };
    match inner {
        Some(inner) => strip_pat(inner),
        None => pat,
    }
}

/// rustc lowers `(e)` on the left of an assignment as `e`, except `(..)`, a tuple pattern.
fn strip_expr(expr: ast::Expr<'_>) -> ast::Expr<'_> {
    let inner = match expr {
        ast::Expr::ParenExpr(it) => it.expr().filter(|it| !is_rest(*it)),
        _ => None,
    };
    match inner {
        Some(inner) => strip_expr(inner),
        None => expr,
    }
}

/// `..` on the left of an assignment.
fn is_rest(expr: ast::Expr<'_>) -> bool {
    matches!(expr, ast::Expr::RangeExpr(it)
        if it.start().is_none()
            && it.end().is_none()
            && it.op_kind() == Some(ast::RangeOp::Exclusive))
}

/// The tuple struct or variant, as rustc's `res` and `variant.fields` give it.
struct Def {
    /// "tuple struct" or "tuple variant": rustc's `res.descr()` of its constructor.
    descr: &'static str,
    /// rustc's `def_span` of the constructor: the struct's or variant's to its name and
    /// generics.
    span: Span,
    /// Its name: rustc's `def_ident_span`.
    ident: Span,
    /// Each field's, as rustc gives a tuple field's index ident: the field after its
    /// attributes.
    field_spans: Vec<Span>,
    fields: Vec<hir::Field>,
}

fn def(cx: &Cx<'_, '_>, variant: hir::Variant) -> Option<Def> {
    let (descr, file, node, name, generics) = match variant {
        hir::Variant::Struct(it) => {
            let src = cx.vsema.source(it)?;
            let it = src.value;
            ("tuple struct", src.file_id, it.syntax(), it.name()?, it.generic_param_list())
        }
        hir::Variant::EnumVariant(it) => {
            let src = cx.vsema.source(it)?;
            let it = src.value;
            ("tuple variant", src.file_id, it.syntax(), it.name()?, None)
        }
        hir::Variant::Union(_) => return None,
    };
    let name = name.syntax().text_range();
    let end = generics.map_or(name.end(), |it| it.syntax().text_range().end().max(name.end()));
    let span = cx.span(InFile::new(file, TextRange::new(item_range(node).start(), end)));
    let fields = variant.fields(cx.db);
    let mut field_spans = Vec::with_capacity(fields.len());
    for &field in &fields {
        let src = cx.vsema.source(field)?;
        field_spans.push(cx.span(src.map(|it| item_range(it.syntax()))));
    }
    Some(Def { descr, span, ident: cx.span(InFile::new(file, name)), field_spans, fields })
}

/// rustc's case of `Some(x, y)` where `Option<(T, U)>` is wanted: one field, of a tuple with
/// as many fields as the pattern has.
fn missing_parentheses<'db>(cx: &Cx<'_, 'db>, pat: &Pat<'db>, fields: &[hir::Field]) -> bool {
    let &[field] = fields else { return false };
    let Some(ty) = &pat.ty else { return false };
    if ty.as_adt().is_none() {
        return false;
    }
    let field_ty = field.ty(cx.db).instantiate_type_params(cx.db, ty.type_arguments());
    field_ty.is_tuple() && field_ty.tuple_fields(cx.db).len() == pat.subpats.len()
}

/// rustc's `is_multiline(a.between(b))`: whether a line ends between `a` and `b`.
fn multiline(cx: &Cx<'_, '_>, a: &Span, b: &Span) -> bool {
    if a.file != b.file {
        return false;
    }
    let between = a.with_lo(a.hi.min(b.lo)).with_hi(a.hi.max(b.lo));
    cx.snippet(&between).is_some_and(|it| it.contains('\n'))
}

fn s(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
