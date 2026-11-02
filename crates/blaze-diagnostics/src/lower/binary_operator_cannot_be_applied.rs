//! E0369 "cannot add `A` to `A`" and E0368 "binary assignment operation `+=` cannot be applied
//! to type `T`": an operator whose trait the operands' types do not implement, with the labels
//! on both operands, the notes on the type that might be missing the impl and on the trait, the
//! derive, bound, dereference and borrow suggestions, and rustc's own forms for `&str + &str` and
//! raw pointers.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/op.rs` (`check_overloaded_binop`,
//! `report_binop_fulfillment_errors`, `suggest_deref_or_call_for_binop_error`,
//! `suggest_deref_binop`, `suggest_different_borrow`, `check_str_addition`,
//! `suggest_raw_ptr_binop_arithmetic`, `suggest_raw_ptr_assign_arithmetic`), `expr.rs`
//! (`check_for_missing_semi`), `method/suggest.rs` (`note_unmet_impls_on_type`,
//! `suggest_derive`, `note_predicate_source_and_get_derives`,
//! `consider_suggesting_derives_for_ty`), `compiler/rustc_trait_selection/src/error_reporting/
//! traits/suggestions.rs` (`suggest_restricting_param_bound`) and
//! `compiler/rustc_middle/src/ty/diagnostics.rs` (`suggest_constraining_type_params`)
//! (MIT or Apache-2.0).

use blaze_items::Fact;
use either::Either;
use hir::{InFile, LangItem};
use rustc_type_ir::inherent::Ty as _;
use syntax::{
    AstPtr, SyntaxKind, SyntaxNodePtr, TextRange, ast as tag,
    view::{self as ast, ArithOp, AstNode as _, BinaryOp, CmpOp, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

pub(crate) fn lower<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::BinaryOperatorCannotBeApplied<'db>,
) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let whole = cx.span_of_ptr(d.expr);
    let bin = match node.value.left() {
        Some(ast::Expr::BinExpr(it)) => Some(it),
        _ => None,
    };
    let lhs_expr = bin.and_then(|it| it.lhs());
    let rhs_expr = bin.and_then(|it| it.rhs());
    let op_span = bin
        .and_then(|it| it.op_token())
        .map_or_else(|| whole.clone(), |it| at(it.text_range())); // clone: the error's span
    let lhs_span = lhs_expr.map(|it| at(it.syntax().text_range()));
    let rhs_span = rhs_expr.map(|it| at(it.syntax().text_range()));
    // clone: the types as rustc prints them as it reports, before integer fallback
    let (lhs, rhs) = match &d.at_error {
        Some((lhs, rhs)) => (lhs.clone(), rhs.clone()),
        None => (cx.ty(&d.lhs), cx.ty(&d.rhs)),
    };
    let op_str = d.op.to_string(); // alloc: the operator, for the error's message
    let trait_ = lang_item(d.op).and_then(|it| hir::Trait::lang(cx.db, cx.krate, it));
    let assign = matches!(d.op, BinaryOp::Assignment { .. });

    let mut diag = match d.op {
        BinaryOp::Assignment { .. } => {
            let mut diag = Diag::error(
                Some("E0368"),
                // alloc: the error's message
                format!("binary assignment operation `{op_str}` cannot be applied to type `{lhs}`"),
                whole.clone(), // clone: the error's span
            );
            if let Some(lhs_span) = &lhs_span {
                // clone: the label's span; alloc: the error's label
                diag.span_label(lhs_span.clone(), format!("cannot use `{op_str}` on type `{lhs}`"));
            }
            if let Some(trait_) = trait_ {
                note_unmet_impl(cx, &mut diag, &d.lhs, trait_, Some(&d.rhs), false);
            }
            diag
        }
        _ => {
            let message = match d.op {
                // alloc: the error's message
                BinaryOp::ArithOp(ArithOp::Add) => format!("cannot add `{rhs}` to `{lhs}`"),
                // alloc: the error's message
                BinaryOp::ArithOp(ArithOp::Sub) => format!("cannot subtract `{rhs}` from `{lhs}`"),
                // alloc: the error's message
                BinaryOp::ArithOp(ArithOp::Mul) => format!("cannot multiply `{lhs}` by `{rhs}`"),
                // alloc: the error's message
                BinaryOp::ArithOp(ArithOp::Div) => format!("cannot divide `{lhs}` by `{rhs}`"),
                BinaryOp::ArithOp(ArithOp::Rem) => {
                    // alloc: the error's message
                    format!("cannot calculate the remainder of `{lhs}` divided by `{rhs}`")
                }
                BinaryOp::ArithOp(
                    ArithOp::BitAnd | ArithOp::BitXor | ArithOp::BitOr | ArithOp::Shl | ArithOp::Shr,
                ) => {
                    // alloc: the error's message
                    format!("no implementation for `{lhs} {op_str} {rhs}`")
                }
                // alloc: the error's message
                _ => format!("binary operation `{op_str}` cannot be applied to type `{lhs}`"),
            };
            let mut diag = Diag::error(Some("E0369"), message, op_span.clone()); // clone: its span
            if let (Some(lhs_span), Some(rhs_span)) = (&lhs_span, &rhs_span)
                && lhs_span != rhs_span
            {
                diag.span_label(lhs_span.clone(), lhs.clone()); // clone: the error's label
                diag.span_label(rhs_span.clone(), rhs.clone()); // clone: the error's label
            }
            if let Some(trait_) = trait_ {
                let suggest_derive = d.lhs.could_unify_with(cx.db, &d.rhs);
                note_unmet_impl(cx, &mut diag, &d.lhs, trait_, Some(&d.rhs), suggest_derive);
            }
            diag
        }
    };

    if let Some(bin) = bin
        && missing_semi(cx, file, bin, &mut diag)
        && is_lhs_of_assign_stmt(bin)
    {
        // rustc leaves this to the error on the assignment, which suggests the semicolon.
        return Vec::new(); // alloc: empty, allocates nothing
    }

    if let (Some(trait_), Some(lhs_expr), Some(rhs_expr)) = (trait_, lhs_expr, rhs_expr) {
        suggest_deref_or_borrow(cx, &mut diag, d, trait_, &op_str, (lhs_expr, rhs_expr), file);
    }

    if let Some(trait_) = trait_ {
        let is_add = matches!(
            d.op,
            BinaryOp::ArithOp(ArithOp::Add) | BinaryOp::Assignment { op: Some(ArithOp::Add) }
        );
        let str_added = is_add
            && match (lhs_expr, rhs_expr) {
                (Some(lhs_expr), Some(rhs_expr)) => {
                    str_addition(cx, &mut diag, d, (lhs_expr, rhs_expr), &op_span, assign, file)
                }
                _ => false,
            };
        if !str_added && has_param(cx, &d.lhs) {
            restrict_param(cx, &mut diag, &d.lhs, trait_, &trait_args(cx, &d.lhs, Some(&d.rhs)));
        }
    }

    if let (Some(lhs_expr), Some(rhs_expr)) = (lhs_expr, rhs_expr) {
        raw_ptr_arithmetic(cx, &mut diag, d, (lhs_expr, rhs_expr), file);
    }

    if let Some(trait_) = trait_ {
        let name = cx.def_path(hir::ModuleDef::Trait(trait_));
        diag.fact_opt(
            cx.implements_fact(trait_, &d.lhs),
            format!("whether `{lhs}` implements `{name}`"), // alloc: the error's fact
        );
    }
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    vec![Found::new(diag, Phase::Bodies)] // alloc: the error found
}

/// The trait of an operator: rustc's `lang_item_for_binop`.
pub(crate) fn lang_item(op: BinaryOp) -> Option<LangItem> {
    let arith = |op: ArithOp, assign: bool| match (op, assign) {
        (ArithOp::Add, false) => LangItem::Add,
        (ArithOp::Sub, false) => LangItem::Sub,
        (ArithOp::Mul, false) => LangItem::Mul,
        (ArithOp::Div, false) => LangItem::Div,
        (ArithOp::Rem, false) => LangItem::Rem,
        (ArithOp::BitXor, false) => LangItem::BitXor,
        (ArithOp::BitAnd, false) => LangItem::BitAnd,
        (ArithOp::BitOr, false) => LangItem::BitOr,
        (ArithOp::Shl, false) => LangItem::Shl,
        (ArithOp::Shr, false) => LangItem::Shr,
        (ArithOp::Add, true) => LangItem::AddAssign,
        (ArithOp::Sub, true) => LangItem::SubAssign,
        (ArithOp::Mul, true) => LangItem::MulAssign,
        (ArithOp::Div, true) => LangItem::DivAssign,
        (ArithOp::Rem, true) => LangItem::RemAssign,
        (ArithOp::BitXor, true) => LangItem::BitXorAssign,
        (ArithOp::BitAnd, true) => LangItem::BitAndAssign,
        (ArithOp::BitOr, true) => LangItem::BitOrAssign,
        (ArithOp::Shl, true) => LangItem::ShlAssign,
        (ArithOp::Shr, true) => LangItem::ShrAssign,
    };
    Some(match op {
        BinaryOp::ArithOp(op) => arith(op, false),
        BinaryOp::Assignment { op: Some(op) } => arith(op, true),
        BinaryOp::CmpOp(CmpOp::Eq { .. }) => LangItem::PartialEq,
        BinaryOp::CmpOp(CmpOp::Ord { .. }) => LangItem::PartialOrd,
        BinaryOp::LogicOp(_) | BinaryOp::Assignment { op: None } => return None,
    })
}

/// The fact of the body a diagnostic is in: the code of the function, const or static whose
/// type checking found it.
pub(crate) fn code_fact(cx: &Cx<'_, '_>, at: InFile<SyntaxNodePtr>, diag: &mut Diag) {
    if let Some((item, name)) = crate::lints::body_item(cx, at) {
        // alloc: the error's fact
        diag.fact(Fact::Code(cx.key(item)), format!("the code of `{name}`"));
    }
}

/// The generic arguments rustc's `print_trait_sugared` shows of an operator's trait: the right
/// side's type, unless it is the default (`Self`, the left side's).
fn trait_args<'db>(cx: &Cx<'_, 'db>, lhs: &hir::Type<'db>, rhs: Option<&hir::Type<'db>>) -> String {
    let Some(rhs) = rhs else { return String::new() }; // alloc: empty, allocates nothing
    let rhs = cx.ty(rhs);
    // alloc: a trait's arguments, for the error
    if rhs == cx.ty(lhs) { String::new() } else { format!("<{rhs}>") }
}

/// rustc's `note_unmet_impls_on_type` of the one predicate an operator failed on,
/// `self_ty: trait_<rhs>`: the note on a local type that it might be missing the impl, or on a
/// foreign one that it does not have it; then the derive that would give it, or the note on
/// the trait.
pub(crate) fn note_unmet_impl<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    self_ty: &hir::Type<'db>,
    trait_: hir::Trait,
    rhs: Option<&hir::Type<'db>>,
    suggest_derive: bool,
) {
    let Some(adt) = self_ty.as_adt() else { return };
    let trait_name = cx.def_path(hir::ModuleDef::Trait(trait_));
    let sugared = format!("{trait_name}{}", trait_args(cx, self_ty, rhs)); // alloc: for the note
    let ty = cx.ty(self_ty);
    let Some(adt_span) = adt_def_span(cx, adt) else { return };
    let local = adt.module(cx.db).krate(cx.db) == cx.krate;
    if local {
        let mut span = MultiSpan::of(adt_span.clone()); // clone: the note's span
        // clone: the label's span; alloc: the note's label
        span.labels.push((adt_span.clone(), format!("must implement `{sugared}`")));
        // alloc: the note
        diag.span_note(span, format!("an implementation of `{sugared}` might be missing for `{ty}`"));
    } else {
        let mut span = MultiSpan::of(adt_span.clone()); // clone: the note's span
        // clone: the label's span; alloc: the note's label
        span.labels.push((adt_span.clone(), format!("`{ty}` is defined in another crate")));
        diag.span_note(span, format!("`{ty}` does not implement `{sugared}`")); // alloc: the note
        return;
    }

    match derivable(cx, adt, self_ty, trait_) {
        Some(traits) => {
            if suggest_derive {
                let traits = traits.join(", ");
                diag.suggestion_verbose(
                    adt_span.shrink_to_lo(),
                    // alloc: the suggestion's message
                    format!("consider annotating `{ty}` with `#[derive({traits})]`"),
                    format!("#[derive({traits})]\n"), // alloc: the suggestion's text
                    Applicability::MaybeIncorrect,
                );
            }
        }
        None => {
            if let Some(trait_span) = trait_def_span(cx, trait_) {
                // alloc: the note
                diag.span_note(trait_span, format!("the trait `{trait_name}` must be implemented"));
            }
        }
    }
}

/// The traits rustc's `consider_suggesting_derives_for_ty` would derive to give `adt` the
/// operator's trait, with its supertraits, in rustc's order; none when the trait can't be
/// derived or `adt` already has an impl of it of its own.
fn derivable<'db>(
    cx: &Cx<'_, 'db>,
    adt: hir::Adt,
    self_ty: &hir::Type<'db>,
    trait_: hir::Trait,
) -> Option<&'static [&'static str]> {
    let lang = |item| hir::Trait::lang(cx.db, cx.krate, item);
    let traits: &'static [&'static str] = if Some(trait_) == lang(LangItem::PartialEq) {
        &["PartialEq"]
    } else if Some(trait_) == lang(LangItem::PartialOrd) {
        &["PartialEq", "PartialOrd"]
    } else {
        return None;
    };
    if matches!(adt, hir::Adt::Union(_)) {
        return None;
    }
    // clone: the type, for the impl lookup on the error path
    let has_own_impl = hir::Impl::all_for_type(cx.db, self_ty.clone())
        .into_iter()
        .any(|it| it.trait_(cx.db) == Some(trait_) && it.self_ty(cx.db).as_adt() == Some(adt));
    (!has_own_impl).then_some(traits)
}

/// rustc's `def_span` of a type.
pub(crate) fn adt_def_span(cx: &Cx<'_, '_>, adt: hir::Adt) -> Option<Span> {
    let range = match adt {
        hir::Adt::Struct(it) => cx.vsema.source(it)?.map(|it| def_range(it.syntax())),
        hir::Adt::Enum(it) => cx.vsema.source(it)?.map(|it| def_range(it.syntax())),
        hir::Adt::Union(it) => cx.vsema.source(it)?.map(|it| def_range(it.syntax())),
    };
    Some(cx.span(range))
}

/// rustc's `def_span` of a trait.
pub(crate) fn trait_def_span(cx: &Cx<'_, '_>, trait_: hir::Trait) -> Option<Span> {
    let node = cx.vsema.source(trait_)?;
    Some(cx.span(node.map(|it| def_range(it.syntax()))))
}

/// rustc's path of an item printed with `with_no_trimmed_paths`: as it is reached from the
/// crate's root (`std::ops::Add`), else where it is defined.
pub(crate) fn full_path(cx: &Cx<'_, '_>, def: hir::ModuleDef) -> String {
    let config = hir::FindPathConfig {
        prefer_no_std: false,
        prefer_prelude: false,
        prefer_absolute: false,
        allow_unstable: true,
    };
    let root = cx.krate.root_module(cx.db);
    if let Some(path) = root.find_path(cx.db, hir::ItemInNs::Types(def), config) {
        return path.display(cx.db, cx.edition).to_string(); // alloc: a path for the error
    }
    def.canonical_path(cx.db, cx.edition).unwrap_or_else(|| cx.def_path(def))
}

/// Whether `ty` has a type parameter in it: rustc's `has_non_region_param`.
pub(crate) fn has_param(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    !ty.generic_params(cx.db).is_empty()
}

/// rustc's `suggest_restricting_param_bound` of `ty: trait_<args>` when `ty` is a type
/// parameter: the bound added where the parameter is declared.
pub(crate) fn restrict_param(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    ty: &hir::Type<'_>,
    trait_: hir::Trait,
    args: &str,
) {
    let Some(param) = ty.as_type_param(cx.db) else { return };
    let Some(source) = cx.vsema.source(param.merge()) else { return };
    let Either::Left(ast::TypeOrConstParam::Type(type_param)) = source.value else { return };
    let Some(name) = type_param.name() else { return };
    let name = name.text();
    let args = args.replace("{integer}", "i32").replace("{float}", "f64");
    // alloc: the suggestion's text
    let constraint = format!("{}{args}", full_path(cx, hir::ModuleDef::Trait(trait_)));
    let trait_name = cx.def_path(hir::ModuleDef::Trait(trait_));
    let post = format!("trait `{trait_name}`"); // alloc: the suggestion's message
    let at = |range: TextRange| cx.span(source.with_value(range));

    let generics = type_param
        .syntax()
        .parent()
        .and_then(|it| it.parent())
        .and_then(ast::AnyHasGenericParams::cast);
    let where_clause = generics.and_then(|it| it.where_clause());
    let where_bounds = where_clause.iter().flat_map(|it| it.predicates()).find_map(|pred| {
        let ty = pred.ty()?;
        (ty.syntax().text() == name).then(|| pred.type_bound_list()?.bounds().last())?
    });
    let inline_last = type_param.type_bound_list().and_then(|it| it.bounds().last());
    let (span, message, code) = if let Some(last) = inline_last.or(where_bounds) {
        (
            at(last.syntax().text_range()).shrink_to_hi(),
            "consider further restricting this bound".to_owned(), // alloc: the message
            format!(" + {constraint}"),                            // alloc: the suggestion's text
        )
    } else if let Some(last) = where_clause.and_then(|it| it.predicates().last()) {
        (
            at(last.syntax().text_range()).shrink_to_hi(),
            // alloc: the suggestion's message
            format!("consider further restricting type parameter `{name}` with {post}"),
            format!(", {name}: {constraint}"), // alloc: the suggestion's text
        )
    } else if type_param.default_type().is_some() {
        // rustc's `where T: Bound` after the generics wants the tail of the item's header,
        // which only a type or trait with a defaulted parameter has.
        return;
    } else if let Some(colon) = type_param.colon_token() {
        (
            at(colon.text_range()).shrink_to_hi(),
            // alloc: the suggestion's message
            format!("consider restricting type parameter `{name}` with {post}"),
            format!(" {constraint}"), // alloc: the suggestion's text
        )
    } else {
        (
            at(type_param.syntax().text_range()).shrink_to_hi(),
            // alloc: the suggestion's message
            format!("consider restricting type parameter `{name}` with {post}"),
            format!(": {constraint}"), // alloc: the suggestion's text
        )
    };
    diag.suggestion_verbose(span, message, code, Applicability::MachineApplicable);
}

/// rustc's `ExprParenthesesNeeded` for an operator rustc's parser took as the start of a new
/// statement after a block-like one (`{ 2 } - 2`, `{ 2 } * x`): whether it was added.
pub(crate) fn parentheses_needed(
    cx: &Cx<'_, '_>,
    expr: InFile<AstPtr<Either<tag::Expr, tag::Pat>>>,
    diag: &mut Diag,
) -> bool {
    let node = cx.node_view(expr);
    let Some(ast::Expr::PrefixExpr(prefix)) = node.value.left() else { return false };
    if !matches!(prefix.op_kind(), Some(ast::UnaryOp::Deref | ast::UnaryOp::Neg)) {
        return false;
    }
    let start = prefix.syntax().text_range().start();
    let mut stmt = prefix.syntax();
    loop {
        let Some(parent) = stmt.parent() else { return false };
        if parent.kind() == SyntaxKind::STMT_LIST {
            break;
        }
        if parent.text_range().start() != start {
            return false;
        }
        stmt = parent;
    }
    let Some(previous) = stmt.prev_sibling().and_then(ast::ExprStmt::cast) else { return false };
    if previous.semicolon_token().is_some() {
        return false;
    }
    let Some(block) = previous.expr() else { return false };
    if !matches!(
        block,
        ast::Expr::BlockExpr(_)
            | ast::Expr::IfExpr(_)
            | ast::Expr::MatchExpr(_)
            | ast::Expr::WhileExpr(_)
            | ast::Expr::LoopExpr(_)
            | ast::Expr::ForExpr(_)
    ) {
        return false;
    }
    let span = cx.span(node.with_value(block.syntax().text_range()));
    diag.multipart_suggestion(
        "parentheses are required to parse this as an expression",
        // alloc: the suggestion's parts
        vec![(span.shrink_to_lo(), "(".to_owned()), (span.shrink_to_hi(), ")".to_owned())],
        Applicability::MachineApplicable,
    );
    true
}

/// rustc's `check_for_missing_semi`: `foo()\n*bar = baz` read as a multiplication.
pub(crate) fn missing_semi<'db>(
    cx: &Cx<'_, 'db>,
    file: hir::HirFileId,
    bin: ast::BinExpr<'db>,
    diag: &mut Diag,
) -> bool {
    if bin.op_kind() != Some(BinaryOp::ArithOp(ArithOp::Mul)) {
        return false;
    }
    let (Some(lhs), Some(rhs)) = (bin.lhs(), bin.rhs()) else { return false };
    let text = bin.syntax().text();
    let base = bin.syntax().text_range().start();
    let between = usize::from(lhs.syntax().text_range().end() - base)
        ..usize::from(rhs.syntax().text_range().start() - base);
    let multiline = text.get(between).is_some_and(|it| it.contains('\n'));
    if !multiline || !is_syntactic_place(cx, rhs) {
        return false;
    }
    let span = cx.span(InFile::new(file, lhs.syntax().text_range())).shrink_to_hi();
    diag.suggestion_verbose(
        span,
        "you might have meant to write a semicolon here",
        ";",
        Applicability::MachineApplicable,
    );
    true
}

/// rustc's `is_syntactic_place_expr`: a local or a static, a dereference, a field or an index.
pub(crate) fn is_syntactic_place<'db>(cx: &Cx<'_, 'db>, expr: ast::Expr<'db>) -> bool {
    match expr {
        ast::Expr::ParenExpr(it) => it.expr().is_some_and(|it| is_syntactic_place(cx, it)),
        ast::Expr::PrefixExpr(it) => it.op_kind() == Some(ast::UnaryOp::Deref),
        ast::Expr::FieldExpr(_) | ast::Expr::IndexExpr(_) => true,
        ast::Expr::PathExpr(it) => it.path().and_then(|it| cx.vsema.resolve_path(it)).is_some_and(
            |it| {
                matches!(
                    it,
                    hir::PathResolution::Local(_)
                        | hir::PathResolution::Def(hir::ModuleDef::Static(_))
                )
            },
        ),
        _ => false,
    }
}

/// rustc's `is_lhs_of_assign_stmt`.
fn is_lhs_of_assign_stmt(bin: ast::BinExpr<'_>) -> bool {
    let Some(parent) = bin.syntax().parent().and_then(ast::BinExpr::cast) else { return false };
    parent.op_kind() == Some(BinaryOp::Assignment { op: None })
        && parent.lhs().is_some_and(|it| it.syntax() == bin.syntax())
        && parent.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::EXPR_STMT)
}

/// The type `ty` dereferences to once, when it can be dereferenced mutably: rustc's
/// `deref_once_mutably_for_diagnostic`.
pub(crate) fn deref_once_mutably<'db>(
    cx: &Cx<'_, 'db>,
    ty: &hir::Type<'db>,
) -> Option<hir::Type<'db>> {
    let target = ty.autoderef(cx.db).nth(1)?;
    let deref_mut = hir::Trait::lang(cx.db, cx.krate, LangItem::DerefMut)?;
    ty.impls_trait(cx.db, deref_mut, &[]).then_some(target)
}

/// rustc's `suggest_deref_or_call_for_binop_error`, of its dereference and borrow suggestions.
fn suggest_deref_or_borrow<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    d: &hir::BinaryOperatorCannotBeApplied<'db>,
    trait_: hir::Trait,
    op_str: &str,
    (lhs_expr, rhs_expr): (ast::Expr<'_>, ast::Expr<'_>),
    file: hir::HirFileId,
) {
    let at = |expr: ast::Expr<'_>| cx.span(InFile::new(file, expr.syntax().text_range()));
    // clone: the type, for the trait query on the error path
    let works = |lhs: &hir::Type<'db>, rhs: &hir::Type<'db>| lhs.impls_trait(cx.db, trait_, &[rhs.clone()]);
    let deref_binop = |diag: &mut Diag, lhs_deref: &hir::Type<'db>| {
        if works(lhs_deref, &d.rhs) {
            diag.suggestion_verbose(
                at(lhs_expr).shrink_to_lo(),
                // alloc: the suggestion's message
                format!(
                    "`{op_str}` can be used on `{}` if you dereference the left-hand side",
                    cx.ty(lhs_deref)
                ),
                "*",
                Applicability::MachineApplicable,
            );
        }
    };
    if matches!(d.op, BinaryOp::Assignment { .. }) {
        if let Some(lhs_deref) = deref_once_mutably(cx, &d.lhs) {
            deref_binop(diag, &lhs_deref);
        }
        return;
    }
    let Some((lhs_deref, lhs_mut)) = d.lhs.as_reference() else { return };
    if lhs_deref.is_copy(cx.db) {
        deref_binop(diag, &lhs_deref);
        return;
    }
    let invert = |m: hir::Mutability| match m {
        hir::Mutability::Shared => hir::Mutability::Mut,
        hir::Mutability::Mut => hir::Mutability::Shared,
    };
    let lhs_inv = invert(lhs_mut);
    let lhs_inv_ty = lhs_deref.add_reference(cx.db, lhs_inv);
    let different_borrow =
        |diag: &mut Diag, lhs: &hir::Type<'db>, lhs_new: Option<hir::Mutability>, rhs: &hir::Type<'db>, rhs_new: Option<hir::Mutability>| {
            if !works(lhs, rhs) {
                return;
            }
            // alloc: the note
            diag.note(format!("an implementation for `{} {op_str} {}` exists", cx.ty(lhs), cx.ty(rhs)));
            if lhs_new == Some(hir::Mutability::Shared) && rhs_new == Some(hir::Mutability::Shared) {
                diag.multipart_suggestion(
                    "consider reborrowing both sides",
                    // alloc: the suggestion's parts
                    vec![
                        // alloc: the suggestion's text
                        (at(lhs_expr).shrink_to_lo(), "&*".to_owned()),
                        // alloc: the suggestion's text
                        (at(rhs_expr).shrink_to_lo(), "&*".to_owned()),
                    ],
                    Applicability::MachineApplicable,
                );
                return;
            }
            for (new, expr) in [(lhs_new, lhs_expr), (rhs_new, rhs_expr)] {
                match new {
                    Some(hir::Mutability::Shared) => {
                        diag.suggestion_verbose(
                            at(expr).shrink_to_lo(),
                            "consider reborrowing this side",
                            "&*",
                            Applicability::MachineApplicable,
                        );
                    }
                    Some(hir::Mutability::Mut) => {
                        diag.span_help(at(expr), "consider making this expression a mutable borrow");
                    }
                    None => {}
                }
            }
        };
    different_borrow(diag, &lhs_inv_ty, Some(lhs_inv), &d.rhs, None);
    if let Some((rhs_deref, rhs_mut)) = d.rhs.as_reference() {
        let rhs_inv = invert(rhs_mut);
        let rhs_inv_ty = rhs_deref.add_reference(cx.db, rhs_inv);
        different_borrow(diag, &d.lhs, None, &rhs_inv_ty, Some(rhs_inv));
        different_borrow(diag, &lhs_inv_ty, Some(lhs_inv), &rhs_inv_ty, Some(rhs_inv));
    }
}

/// rustc's `check_str_addition`: `&str + &str` and `&str + String`, whether it applied.
fn str_addition<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    d: &hir::BinaryOperatorCannotBeApplied<'db>,
    (lhs_expr, rhs_expr): (ast::Expr<'_>, ast::Expr<'_>),
    op_span: &Span,
    assign: bool,
    file: hir::HirFileId,
) -> bool {
    const STR_CONCAT_NOTE: &str = "string concatenation requires an owned `String` on the left";
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let string = hir_def::lang_item::lang_items(cx.db, cx.krate.base()).String;
    let is_string = |ty: &hir::Type<'db>| {
        string.is_some_and(|it| ty.as_adt() == Some(hir::Adt::Struct(it.into())))
    };
    let is_str_like = |ty: &hir::Type<'db>| ty.is_str() || is_string(ty);
    // Where the left side is made owned: the borrow removed, or `.to_owned()` after it.
    let owned = || match lhs_expr {
        ast::Expr::RefExpr(it) => {
            let inner = it.expr().map_or(it.syntax().text_range().end(), |it| it.syntax().text_range().start());
            (at(TextRange::new(it.syntax().text_range().start(), inner)), None)
        }
        // alloc: the suggestion's text
        _ => (at(lhs_expr.syntax().text_range()).shrink_to_hi(), Some(".to_owned()".to_owned())),
    };
    let Some((l_ty, _)) = d.lhs.as_reference() else { return false };
    if !is_str_like(&l_ty) {
        return false;
    }
    let rhs_ref = d.rhs.as_reference().map(|(it, _)| it);
    if let Some(r_ty) = &rhs_ref
        && (is_str_like(r_ty) || r_ty.as_reference().is_some_and(|(it, _)| it.is_str()))
    {
        if !assign {
            // clone: the label's span
            diag.span_label(op_span.clone(), "`+` cannot be used to concatenate two `&str` strings");
            diag.note(STR_CONCAT_NOTE);
            let (span, replacement) = owned();
            let (message, replacement) = match replacement {
                // alloc: empty, allocates nothing
                None => ("remove the borrow to obtain an owned `String`", String::new()),
                Some(it) => ("create an owned `String` from a string reference", it),
            };
            diag.suggestion_verbose(span, message, replacement, Applicability::MachineApplicable);
        }
        return true;
    }
    if rhs_ref.is_none() && is_string(&d.rhs) {
        // clone: the label's span
        diag.span_label(op_span.clone(), "`+` cannot be used to concatenate a `&str` with a `String`");
        if !assign {
            let (span, replacement) = owned();
            let (message, replacement) = match replacement {
                // alloc: empty, allocates nothing
                None => ("remove the borrow on the left and add one on the right", String::new()),
                Some(it) => ("create an owned `String` on the left and add a borrow on the right", it),
            };
            diag.multipart_suggestion(
                message,
                // alloc: the suggestion's parts
                vec![
                    (span, replacement),
                    // alloc: the suggestion's text
                    (at(rhs_expr.syntax().text_range()).shrink_to_lo(), "&".to_owned()),
                ],
                Applicability::MachineApplicable,
            );
        } else {
            diag.note(STR_CONCAT_NOTE);
        }
        return true;
    }
    false
}

/// rustc's `suggest_raw_ptr_binop_arithmetic` and `suggest_raw_ptr_assign_arithmetic`.
fn raw_ptr_arithmetic<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    d: &hir::BinaryOperatorCannotBeApplied<'db>,
    (lhs_expr, rhs_expr): (ast::Expr<'_>, ast::Expr<'_>),
    file: hir::HirFileId,
) {
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let lhs_range = lhs_expr.syntax().text_range();
    let rhs_range = rhs_expr.syntax().text_range();
    let between = at(TextRange::new(lhs_range.end(), rhs_range.start()));
    let rhs_end = at(rhs_range).shrink_to_hi();
    let rhs_integral = d.rhs.to_next_solver().is_integral();
    let lhs_ptr = d.lhs.is_raw_ptr();
    match d.op {
        BinaryOp::ArithOp(ArithOp::Add) if lhs_ptr && rhs_integral => {
            diag.multipart_suggestion(
                "consider using `wrapping_add` or `add` for pointer + {integer}",
                // alloc: the suggestion's parts
                vec![(between, ".wrapping_add(".to_owned()), (rhs_end, ")".to_owned())],
                Applicability::MaybeIncorrect,
            );
        }
        BinaryOp::ArithOp(ArithOp::Sub) => {
            if lhs_ptr && rhs_integral {
                diag.multipart_suggestion(
                    "consider using `wrapping_sub` or `sub` for pointer - {integer}",
                    // clone: the suggestion's spans; alloc: its parts
                    vec![(between.clone(), ".wrapping_sub(".to_owned()), (rhs_end.clone(), ")".to_owned())],
                    Applicability::MaybeIncorrect,
                );
            }
            if lhs_ptr && d.rhs.is_raw_ptr() {
                diag.multipart_suggestion(
                    "consider using `offset_from` for pointer - pointer if the pointers point to the same allocation",
                    // alloc: the suggestion's parts
                    vec![
                        (at(lhs_range).shrink_to_lo(), "unsafe { ".to_owned()), // alloc: its text
                        (between, ".offset_from(".to_owned()),                  // alloc: its text
                        (rhs_end, ") }".to_owned()),                            // alloc: its text
                    ],
                    Applicability::MaybeIncorrect,
                );
            }
        }
        BinaryOp::Assignment { op: Some(op @ (ArithOp::Add | ArithOp::Sub)) } if lhs_ptr && rhs_integral => {
            let (message, method) = match op {
                ArithOp::Add => ("consider using `add` or `wrapping_add` to do pointer arithmetic", "wrapping_add"),
                _ => ("consider using `sub` or `wrapping_sub` to do pointer arithmetic", "wrapping_sub"),
            };
            let name = match lhs_expr {
                ast::Expr::PathExpr(it) => it
                    .path()
                    .and_then(|it| it.segment())
                    .and_then(|it| it.name_ref())
                    .map_or_else(|| "_".to_owned(), |it| it.text().to_owned()), // alloc: its text
                _ => lhs_expr.syntax().text().to_owned(), // alloc: the suggestion's text
            };
            diag.multipart_suggestion(
                message,
                // alloc: the suggestion's parts
                vec![
                    (at(lhs_range).shrink_to_lo(), format!("{name} = ")), // alloc: its text
                    (between, format!(".{method}(")),                     // alloc: its text
                    (rhs_end, ")".to_owned()),                            // alloc: its text
                ],
                Applicability::MaybeIncorrect,
            );
        }
        _ => {}
    }
}
