//! The discriminants of an enum: E0370 for one implied past the end of its `repr` type, E0732
//! for explicit discriminants with non-unit variants and no `repr` integer type, E0081 for a
//! value assigned twice, E0084 for a `repr` of an enum with no variants.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/collect.rs` (`check_enum_variant_types`,
//! `EnumDiscriminantOverflowed`) and `check/check.rs` (`check_enum`,
//! `detect_discriminant_duplicate`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_ty::consteval::discriminants::{
    DiscrInt, EnumDiscriminants, enum_discriminants, explicit_discriminant_indices,
};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange,
    view::{self as ast, AstNode as _, HasAttrs as _, HasName as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{Found, Phase},
    model::{Diag, Span},
};

/// The span of a variant's name, or of the variant without one.
fn name_span(cx: &Cx<'_, '_>, node: InFile<ast::Variant<'_>>) -> Span {
    match node.value.name() {
        Some(it) => cx.span(node.with_value(it.syntax().text_range())),
        None => cx.span(node.map(|it| it.syntax().text_range())),
    }
}

/// The expression assigning a variant's discriminant.
fn discr_expr(node: ast::Variant<'_>) -> Option<ast::Expr<'_>> {
    node.const_arg().and_then(|it| it.expr())
}

pub(super) fn check(cx: &Cx<'_, '_>, enum_: hir::Enum) -> Vec<Found> {
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(enum_) else { return Vec::new() };
    let enum_span = || cx.span(source.map(|it| def_range(it.syntax())));
    // alloc: the errors' fact
    let what = || format!("the variants of `{}`", cx.def_path(hir::ModuleDef::Adt(enum_.into())));
    let fact = Fact::Layout(cx.key(ItemId::Adt(hir::Adt::from(enum_).into())));
    // alloc: empty unless there is an error
    let mut found = Vec::new();
    let variants = enum_.variants(cx.db);
    if variants.is_empty() {
        if let Some(hint) = first_repr_hint(source.value) {
            let span = cx.span(source.with_value(hint));
            let mut diag =
                Diag::error(Some("E0084"), "unsupported representation for zero-variant enum", span);
            diag.span_label(enum_span(), "zero-variant enum");
            diag.fact(fact, what());
            found.push(Found::new(diag, Phase::Items));
        }
        return found;
    }
    let needs_repr = needs_repr_int(cx, enum_, &variants, source);
    let id = hir_def::EnumId::from(enum_);
    let table = enum_discriminants(cx.db, id);
    let Some(int) = table.int() else {
        found.extend(needs_repr.map(|mut diag| {
            // clone: the error's fact
            diag.fact(fact.clone(), what());
            Found::new(diag, Phase::Items)
        }));
        return found;
    };
    for &index in table.overflowed() {
        let index = index as usize;
        let Some(node) = cx.vsema.source(variants[index]) else { continue };
        let name_span = name_span(cx, node);
        // clone: the error's span
        let mut diag = Diag::error(Some("E0370"), "enum discriminant overflowed", name_span.clone());
        let after = int.display(table.value(index - 1));
        // alloc: the error's label
        diag.span_label(name_span, format!("overflowed on value after {after}"));
        let name = variants[index].name(cx.db);
        // alloc: the error's note
        diag.note(format!(
            "explicitly set `{} = {}` if that is desired outcome",
            name.as_str(),
            int.display(table.value(index))
        ));
        // clone: the error's fact
        diag.fact(fact.clone(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    if let Some(mut diag) = needs_repr {
        // clone: the error's fact
        diag.fact(fact.clone(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    let explicit = explicit_discriminant_indices(cx.db, id);
    for group in table.duplicates() {
        let first = int.display(table.value(group[0] as usize));
        // alloc: the error's message
        let message = format!("discriminant value `{first}` assigned more than once");
        let mut diag = Diag::error(Some("E0081"), message, enum_span());
        for &index in &group {
            label(cx, &mut diag, &variants, explicit, table, index as usize);
        }
        // clone: the error's fact
        diag.fact(fact.clone(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// `check_enum`'s E0732: an enum without a `repr` integer type whose non-unit variant has an
/// explicit discriminant, or that has an explicit discriminant and a non-unit variant.
fn needs_repr_int(
    cx: &Cx<'_, '_>,
    enum_: hir::Enum,
    variants: &[hir::EnumVariant],
    source: InFile<ast::Enum<'_>>,
) -> Option<Diag> {
    if enum_.repr(cx.db).is_some_and(|it| it.int.is_some()) {
        return None;
    }
    // The first non-unit variant, and the first explicit discriminant of a unit and of a
    // non-unit variant.
    let mut non_unit: Option<InFile<ast::Variant<'_>>> = None;
    let mut disr_unit: Option<InFile<ast::Expr<'_>>> = None;
    let mut disr_non_unit: Option<InFile<ast::Expr<'_>>> = None;
    for &variant in variants {
        let Some(node) = cx.vsema.source(variant) else { continue };
        let unit = variant.kind(cx.db) == hir::StructKind::Unit;
        let disr = if unit { &mut disr_unit } else { &mut disr_non_unit };
        if disr.is_none()
            && let Some(expr) = discr_expr(node.value)
        {
            *disr = Some(node.with_value(expr));
        }
        if !unit && non_unit.is_none() {
            non_unit = Some(node);
        }
    }
    if disr_non_unit.is_none() && !(disr_unit.is_some() && non_unit.is_some()) {
        return None;
    }
    let expr_span = |expr: InFile<ast::Expr<'_>>| cx.span(expr.map(|it| it.syntax().text_range()));
    let mut diag = Diag::error(
        Some("E0732"),
        "`#[repr(inttype)]` must be specified for enums with explicit discriminants and non-unit variants",
        cx.span(source.map(|it| def_range(it.syntax()))),
    );
    match (disr_non_unit, disr_unit, non_unit) {
        (Some(at), _, _) => {
            diag.span_label(expr_span(at), "explicit discriminant on non-unit variant specified here");
        }
        (None, Some(at), Some(non_unit)) => {
            diag.span_label(expr_span(at), "explicit discriminant specified here");
            diag.span_label(name_span(cx, non_unit), "non-unit discriminant declared here");
        }
        _ => {}
    }
    Some(diag)
}

/// rustc's `report` in `detect_discriminant_duplicate`: where variant `index` gets its value.
/// `explicit` is the variants with an explicit discriminant, ascending.
fn label(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    variants: &[hir::EnumVariant],
    explicit: &[u32],
    table: &EnumDiscriminants<'_>,
    index: usize,
) {
    let Some(node) = cx.vsema.source(variants[index]) else { return };
    let Some(int) = table.int() else { return };
    let value = table.value(index);
    let shown = int.display(value);
    let (span, shown) = match discr_expr(node.value) {
        Some(expr) => {
            let span = cx.span(node.with_value(expr.syntax().text_range()));
            match literal(expr) {
                Some(literal) if literal != bits(int, value) => {
                    // alloc: the error's label
                    (span, format!("`{shown}` (overflowed from `{literal}`)"))
                }
                // alloc: the error's label
                _ => (span, format!("`{shown}`")),
            }
        }
        None => {
            let start = match explicit.partition_point(|&at| at as usize <= index) {
                0 => 0,
                after => explicit[after - 1] as usize,
            };
            let distance = index - start;
            if distance > 0
                && let Some(start_node) = cx.vsema.source(variants[start])
            {
                let plural = if distance > 1 { "variants" } else { "variant" };
                let (name, start_name) = (variants[index].name(cx.db), variants[start].name(cx.db));
                diag.span_label(
                    name_span(cx, start_node),
                    // alloc: the error's label
                    format!(
                        "discriminant for `{}` incremented from this startpoint (`{}` + {distance} {plural} later => `{}` = {shown})",
                        name.as_str(),
                        start_name.as_str(),
                        name.as_str()
                    ),
                );
            }
            // alloc: the error's label
            (name_span(cx, node), format!("`{shown}`"))
        }
    };
    // alloc: the error's label
    diag.span_label(span, format!("{shown} assigned here"));
}

/// `value`'s bits in the type, as rustc's `Discr::val` holds them.
fn bits(int: DiscrInt, value: i128) -> u128 {
    let mask = if int.bits >= 128 { u128::MAX } else { (1u128 << int.bits) - 1 };
    value as u128 & mask
}

/// The value of an integer literal, as rustc's check reads one.
fn literal(expr: ast::Expr<'_>) -> Option<u128> {
    let ast::Expr::Literal(literal) = expr else { return None };
    match literal.kind() {
        ast::LiteralKind::IntNumber(it) => it.value().ok(),
        _ => None,
    }
}

/// The span of the first hint of the first `#[repr(..)]` of `node`: `i32` of `#[repr(i32)]`.
fn first_repr_hint(node: ast::Enum<'_>) -> Option<TextRange> {
    let attr = node.attrs().find(|it| it.simple_name() == Some("repr"))?;
    let (_, tt) = attr.meta()?.as_simple_call()?;
    let mut start = None;
    let mut end = None;
    let mut depth = 0usize;
    for it in tt.syntax().children_with_tokens() {
        let range = it.text_range();
        match it {
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::L_PAREN && depth == 0 => {
                depth = 1;
                continue;
            }
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::R_PAREN && depth == 1 => break,
            NodeOrToken::Token(token) if token.kind() == SyntaxKind::COMMA => break,
            NodeOrToken::Token(token) if token.kind().is_trivia() => continue,
            _ => {}
        }
        start.get_or_insert(range.start());
        end = Some(range.end());
    }
    Some(TextRange::new(start?, end?))
}
