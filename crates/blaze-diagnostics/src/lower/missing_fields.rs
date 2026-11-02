//! The fields a struct expression or pattern leaves out: E0063 (`S { a: 1 }` of a struct with
//! a field `b`), its form for a struct with fields not visible where it is built (`cannot
//! construct `S` with struct literal syntax due to private fields`), the forms of `..` with
//! no base (E0797 without `#![feature(default_field_values)]`, `missing field `b` in
//! initializer` with it), and of a pattern E0027 (`pattern does not mention field `b``), its
//! form when every field left out is inaccessible (`pattern requires `..` due to
//! inaccessible fields`) and E0769 in place of it for a tuple struct or variant written with
//! names.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/expr.rs` (`check_expr_struct_fields`,
//! `report_missing_fields`, `suggest_fru_from_range_and_emit`, `report_private_fields`),
//! `compiler/rustc_hir_typeck/src/diagnostics.rs` (`BaseExpressionDoubleDot`,
//! `TypeMismatchFruTypo`) and `compiler/rustc_hir_typeck/src/pat.rs`
//! (`check_struct_pat_fields`, `error_unmentioned_fields`, `error_no_accessible_fields`)
//! (MIT or Apache-2.0).

use blaze_ids::radix;
use blaze_items::{Fact, ItemId};
use hir::{HasAttrs as _, HasVisibility as _, InFile, LangItem};
use either::Either;
use syntax::{
    SyntaxKind, T, TextRange, TextSize,
    view::{self as ast, AstNode as _, RangeItem as _},
};

use stdx::list::Join;
use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{
        Found, Phase,
        no_such_field::{
            Record, RecordField, has_unmentioned_error, is_number, pat_fields, suggestable, tuple_variant_as_struct,
            variant_path,
        },
        unresolved_method_call::inject_use_span,
    },
    model::{Applicability, Diag, MultiSpan, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingFields) -> Vec<Found> {
    let variant = d.variant;
    let node = cx.node_view(InFile::new(d.file, d.field_list_parent));
    // alloc: empty, allocates nothing
    let Some(record) = Record::of(d.file, node.value) else { return vec![] };
    let destructuring = record.is_pat && record.node.kind() == SyntaxKind::RECORD_EXPR;
    if !destructuring && recovered(&record) {
        // alloc: empty, allocates nothing
        return vec![];
    }
    let found = match record.is_pat {
        true => pattern(cx, &record, variant),
        false => expression(cx, &record, variant),
    };
    // alloc: empty, allocates nothing
    let Some(mut diag) = found else { return vec![] };
    adt_fact(cx, &mut diag, variant);
    code_fact(cx, &mut diag, InFile::new(record.file, ast::ptr(record.node)));
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// Whether the parser recovered from an error in the fields, where rustc reports no field
/// missing (`StructTailExpr::NoneWithError`, `PatFieldsRest::Recovered`): the field it could
/// not read may be the one missing. A destructuring assignment is lowered to a pattern
/// without it (`destructure_assign_mut`), so it still reports them.
fn recovered(record: &Record<'_>) -> bool {
    let list = record
        .node
        .children()
        .find(|it| matches!(it.kind(), SyntaxKind::RECORD_EXPR_FIELD_LIST | SyntaxKind::RECORD_PAT_FIELD_LIST));
    let Some(list) = list else { return true };
    let closed = list.children_with_tokens().any(|it| it.kind() == T!['}']);
    let error = list.descendants_with_tokens().any(|it| it.kind() == SyntaxKind::ERROR);
    let fields = list
        .children()
        .filter(|it| matches!(it.kind(), SyntaxKind::RECORD_EXPR_FIELD | SyntaxKind::RECORD_PAT_FIELD))
        .count();
    !closed || error || fields != record.fields.len()
}

fn adt_fact(cx: &Cx<'_, '_>, diag: &mut Diag, variant: hir::Variant) {
    let adt = variant.adt(cx.db);
    let name = variant_path(cx, variant);
    // alloc: the error's text, for an error hir found
    diag.fact(Fact::Signature(cx.key(ItemId::Adt(adt.into()))), format!("the fields of `{name}`"));
}

/// rustc's `adt_ty` as it prints it: the type of the expression, `Enum` for one of its
/// variants.
fn adt_ty<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant) -> String {
    match record.ty(cx) {
        Some(ty) if !ty.contains_unknown() => cx.ty(&ty),
        _ => cx.def_path(hir::ModuleDef::Adt(variant.adt(cx.db))),
    }
}

/// Whether a field has a default value (`a: u8 = 1`).
fn has_default(cx: &Cx<'_, '_>, field: hir::Field) -> bool {
    match cx.vsema.source(field).map(|it| it.value) {
        Some(hir::FieldSourceView::Named(it)) => it.default_val().is_some(),
        _ => false,
    }
}

/// rustc's `field_list_has_applicable_non_exhaustive`: `#[non_exhaustive]` and of another
/// crate.
fn foreign_non_exhaustive(cx: &Cx<'_, '_>, variant: hir::Variant) -> bool {
    let marked = match variant {
        hir::Variant::Struct(it) => it.attrs(cx.db).is_non_exhaustive(),
        hir::Variant::Union(it) => it.attrs(cx.db).is_non_exhaustive(),
        hir::Variant::EnumVariant(it) => it.attrs(cx.db).is_non_exhaustive(),
    };
    marked && variant.adt(cx.db).module(cx.db).krate(cx.db) != cx.krate
}

/// The fields of `variant` the expression leaves out, in the order of the variant.
fn remaining(cx: &Cx<'_, '_>, record: &Record<'_>, variant: hir::Variant) -> Vec<(String, hir::Field)> {
    variant
        .fields_iter(cx.db)
        // alloc: the error's parts, for an error hir found
        .map(|it| (it.name(cx.db).as_str().to_owned(), it))
        .filter(|(name, _)| record.fields.iter().all(|it| &it.name != name))
        // alloc: the error's parts, for an error hir found
        .collect()
}

/// The struct expression forms (`check_expr_struct_fields`).
fn expression<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant) -> Option<Diag> {
    // alloc: the error's parts, for an error hir found
    let names: Vec<String> = variant.fields_iter(cx.db).map(|it| it.name(cx.db).as_str().to_owned()).collect();
    let unknown = record.fields.iter().any(|it| !names.contains(&it.name));
    let repeated = record.repeats().into_iter().any(|it| it);
    if unknown || repeated {
        return None;
    }
    let remaining = remaining(cx, record, variant);
    match record.rest {
        Some((_, Some(_))) => None,
        Some((dots, None)) => default_fields(cx, record, variant, dots, &remaining),
        None => {
            if matches!(variant, hir::Variant::Union(_))
                || remaining.is_empty()
                || foreign_non_exhaustive(cx, variant)
            {
                return None;
            }
            let private = private(cx, record, variant);
            Some(match private.is_empty() {
                true => missing(cx, record, variant, &remaining),
                false => private_fields(cx, record, variant, &private),
            })
        }
    }
}

/// The fields of `variant` not visible where the expression is.
fn private<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant) -> Vec<hir::Field> {
    let module = record.module(cx);
    variant
        .fields_iter(cx.db)
        .filter(|it| module.is_some_and(|module| !it.is_visible_from(cx.db, module)))
        // alloc: the error's parts, for an error hir found
        .collect()
}

/// rustc's `StashKey::MaybeFruTypo`: what becomes of the E0308 `diag` of `value`, when it is
/// the value of the last field of a struct expression. With no field left out it gets the
/// range note itself (`suggest_fru_from_range_and_emit` of the E0308); with some it is
/// stashed, and the E0063 that gives the range note takes its place
/// (`try_steal_replace_and_emit_err`). False when it is not to be given.
pub(crate) fn last_field_mismatch<'db>(cx: &Cx<'_, 'db>, value: InFile<ast::Node<'db>>, diag: &mut Diag) -> bool {
    let Some(field) = value.value.parent().and_then(ast::RecordExprField::cast) else { return true };
    let Some(record) = Record::of_field(value.file_id, Either::Left(field)) else { return true };
    let Some(last) = record.fields.last() else { return true };
    if record.is_pat || last.value != Some(value.value) {
        return true;
    }
    let Some(variant) = cx.vsema.resolve_variant(field.parent_record_lit()) else { return true };
    let remaining = remaining(cx, &record, variant);
    if remaining.is_empty() {
        fru_typo(cx, diag, &record, last, variant);
        return true;
    }
    // alloc: the error's parts, for an error hir found
    let names: Vec<String> = variant.fields_iter(cx.db).map(|it| it.name(cx.db).as_str().to_owned()).collect();
    let gives_e0063 = record.rest.is_none()
        && !record.fields.iter().any(|it| !names.contains(&it.name))
        && !record.repeats().into_iter().any(|it| it)
        && !matches!(variant, hir::Variant::Union(_))
        && !foreign_non_exhaustive(cx, variant)
        && private(cx, &record, variant).is_empty();
    !(gives_e0063 && fru_range(cx, last, variant).is_some())
}

/// `..` with no base: E0797, or with `#![feature(default_field_values)]` the fields without
/// a default value, or the variant having no fields.
fn default_fields<'db>(
    cx: &Cx<'_, 'db>,
    record: &Record<'db>,
    variant: hir::Variant,
    dots: TextRange,
    remaining: &[(String, hir::Field)],
) -> Option<Diag> {
    let mandatory: Vec<&str> =
        // alloc: the error's parts, for an error hir found
        remaining.iter().filter(|(_, it)| !has_default(cx, *it)).map(|(name, _)| name.as_str()).collect();
    let feature = cx.krate.is_unstable_feature_enabled(cx.db, &intern::Symbol::intern("default_field_values"));
    let dots = record.span(cx, dots);
    if !feature {
        let at = dots.shrink_to_hi();
        // clone: the error's parts, for an error hir found
        let mut diag = Diag::error(Some("E0797"), "base expression required after `..`", at.clone());
        // `sess.is_nightly_build()`, as `checks::features` reads it: `RUSTC_BOOTSTRAP=-1` makes
        // it a stable compiler.
        let nightly = cx.krate.base().env(cx.db).get("RUSTC_BOOTSTRAP").as_deref() != Some("-1");
        if nightly && mandatory.is_empty() && !remaining.is_empty() {
            let root = cx.krate.root_module(cx.db);
            let inject = inject_use_span(cx, root).expect("the crate root is a file, whose items have a start");
            diag.suggestion_verbose(
                inject,
                "add `#![feature(default_field_values)]` to the crate attributes to enable default values on `struct` fields",
                "#![feature(default_field_values)]\n",
                Applicability::MachineApplicable,
            );
        }
        match remaining.is_empty() {
            false => {
                diag.suggestion_verbose(at, "add a base expression here", "/* expr */", Applicability::HasPlaceholders)
            }
            true => diag.suggestion_verbose(
                dots,
                "remove the `..` as all the fields are already present",
                "",
                Applicability::MachineApplicable,
            ),
        };
        return Some(diag);
    }
    if variant.fields_iter(cx.db).next().is_none() {
        let message =
            // alloc: the error's text, for an error hir found
            format!("`{}` has no fields, `..` needs at least one default field in the struct definition", adt_ty(cx, record, variant));
        let mut diag = Diag::error(None, message, dots);
        diag.span_label(record.span(cx, record.path_range()), "this type has no fields");
        return Some(diag);
    }
    if mandatory.is_empty() {
        return None;
    }
    let s = if mandatory.len() == 1 { "" } else { "s" };
    let at = dots.shrink_to_lo();
    // alloc: clone: the error's parts, for an error hir found
    let mut diag = Diag::error(None, format!("missing field{s} {} in initializer", stdx::list::list(mandatory.iter(), Join::AndNoSerialComma, true)), at.clone());
    diag.span_label(at, "fields that do not have a defaulted value must be provided explicitly");
    Some(diag)
}

/// E0063 (`report_missing_fields`).
fn missing<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant, remaining: &[(String, hir::Field)]) -> Diag {
    // alloc: the error's parts, for an error hir found
    let mut sorted: Vec<&str> = remaining.iter().map(|(name, _)| name.as_str()).collect();
    sorted.sort_unstable();
    let len = sorted.len();
    let (names, truncated) = match sorted.as_slice() {
        // alloc: the error's text, for an error hir found
        [a] => (format!("`{a}`"), String::new()),
        // alloc: the error's text, for an error hir found
        [a, b] => (format!("`{a}` and `{b}`"), String::new()),
        // alloc: the error's text, for an error hir found
        [a, b, c] => (format!("`{a}`, `{b}` and `{c}`"), String::new()),
        _ => {
            // alloc: the error's text, for an error hir found
            let shown: Vec<String> = sorted.iter().take(3).map(|it| format!("`{it}`")).collect();
            let others = len - 3;
            // alloc: the error's text, for an error hir found
            (shown.join(", "), format!(" and {others} other field{}", if others == 1 { "" } else { "s" }))
        }
    };
    let s = if len == 1 { "" } else { "s" };
    let path = record.span(cx, record.path_range());
    // alloc: the error's text, for an error hir found
    let message = format!("missing field{s} {names}{truncated} in initializer of `{}`", adt_ty(cx, record, variant));
    // clone: the error's parts, for an error hir found
    let mut diag = Diag::error(Some("E0063"), message, path.clone());
    // alloc: clone: the error's parts, for an error hir found
    diag.span_label(path.clone(), format!("missing {names}{truncated}"));
    if remaining.iter().all(|(_, it)| has_default(cx, *it)) {
        let feature = cx.krate.is_unstable_feature_enabled(cx.db, &intern::Symbol::intern("default_field_values"));
        let you_can = match feature {
            true => "you can",
            false => "if you added `#![feature(default_field_values)]` to your crate you could",
        };
        // alloc: the error's text, for an error hir found
        let message = format!("all remaining fields have default values, {you_can} use those values with `..`");
        match record.fields.last() {
            Some(last) => {
                let at = record.span(cx, last.range).shrink_to_hi();
                diag.suggestion_verbose(at, message, ", ..", Applicability::MachineApplicable);
            }
            None => {
                let at = record.span(cx, TextRange::new(record.path_range().end(), record.node.text_range().end()));
                diag.suggestion_verbose(at, message, " { .. }", Applicability::MachineApplicable);
            }
        }
    }
    if let Some(last) = record.fields.last() {
        fru_typo(cx, &mut diag, record, last, variant);
    }
    diag
}

/// The last field's value when it is a range `a..b` of a field that is not a `Range`, and
/// its two ends: the condition of `suggest_fru_from_range_and_emit`.
fn fru_range<'db>(
    cx: &Cx<'_, 'db>,
    last: &RecordField<'db>,
    variant: hir::Variant,
) -> Option<(ast::RangeExpr<'db>, ast::Expr<'db>, ast::Expr<'db>)> {
    let range = last.value.and_then(ast::RangeExpr::cast)?;
    let (Some(start), Some(end)) = (range.start(), range.end()) else { return None };
    if range.op_kind() != Some(ast::RangeOp::Exclusive) {
        return None;
    }
    let range_adt = cx.vsema.type_of_expr(ast::Expr::RangeExpr(range)).and_then(|it| it.original.as_adt());
    let field_adt = variant
        .fields_iter(cx.db)
        .find(|it| it.name(cx.db).as_str() == last.name)
        .and_then(|it| it.ty(cx.db).as_adt());
    if range_adt.is_none() || field_adt == range_adt {
        return None;
    }
    Some((range, start, end))
}

/// `TypeMismatchFruTypo` (`suggest_fru_from_range_and_emit`): the last field's value is a
/// range `a..b` where the field is not a `Range`, likely `a, ..b` meant.
fn fru_typo<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    record: &Record<'db>,
    last: &RecordField<'db>,
    variant: hir::Variant,
) {
    let Some((range, start, end)) = fru_range(cx, last, variant) else { return };
    let start_range = start.syntax().text_range();
    let end_range = end.syntax().text_range();
    let text = range.syntax().text();
    let between = text.get(usize::from(start_range.end() - range.syntax().text_range().start())..).unwrap_or_default();
    let spaces: usize = between.chars().take_while(|it| it.is_whitespace()).map(char::len_utf8).sum();
    let fru_lo = start_range.end() + TextSize::try_from(spaces).unwrap_or_default();
    let expr_span = record.span(cx, start_range);
    let fru_span = record.span(cx, TextRange::new(fru_lo.min(end_range.end()), end_range.end()));
    let note = "this expression may have been misinterpreted as a `..` range expression";
    if spaces == 0 {
        diag.span_note(expr_span.to(&fru_span), note);
    } else {
        // alloc: the error's parts, for an error hir found
        let labels = vec![
            // alloc: clone: the error's parts, for an error hir found
            (expr_span.clone(), "this expression does not end in a comma...".to_owned()),
            (
                // clone: the error's parts, for an error hir found
                fru_span.clone(),
                "... so this is interpreted as a `..` range expression, instead of functional record update syntax"
                    // alloc: the error's text, for an error hir found
                    .to_owned(),
            ),
        ];
        // alloc: clone: the error's parts, for an error hir found
        diag.span_note(MultiSpan { primary: vec![expr_span.clone(), fru_span], labels }, note);
    }
    let shown = cx
        .snippet(&record.span(cx, end_range))
        .filter(|it| it.len() < 25 && !it.contains(|c: char| c.is_control()))
        // alloc: the error's text, for an error hir found
        .map(|it| format!(" from `{it}`"))
        .unwrap_or_default();
    diag.suggestion(
        expr_span.shrink_to_hi(),
        // alloc: the error's text, for an error hir found
        format!("to set the remaining fields{shown}, separate the last named field with a comma"),
        ", ",
        Applicability::MaybeIncorrect,
    );
}

/// `report_private_fields`: a struct with a field not visible where it is built.
fn private_fields<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant, private: &[hir::Field]) -> Diag {
    let adt_ty_text = adt_ty(cx, record, variant);
    let path = record.span(cx, record.path_range());
    let mut diag = Diag::error(
        None,
        // alloc: the error's text, for an error hir found
        format!("cannot construct `{adt_ty_text}` with struct literal syntax due to private fields"),
        // clone: the error's parts, for an error hir found
        path.clone(),
    );
    // alloc: the error's parts, for an error hir found
    let mut left_out = Vec::new();
    for field in private {
        // alloc: the error's text, for an error hir found
        let name = field.name(cx.db).as_str().to_owned();
        match record.fields.iter().find(|it| it.name == name) {
            Some(used) => {
                diag.span_label(record.span(cx, used.range), "private field");
            }
            None => left_out.push(name),
        }
    }
    let adt = variant.adt(cx.db);
    let local = adt.module(cx.db).krate(cx.db) == cx.krate;
    if (local || !record.fields.is_empty()) && !left_out.is_empty() {
        let names = match left_out.len() > 6 {
            // alloc: empty, allocates nothing
            true => String::new(),
            false => {
                // alloc: the error's parts, for an error hir found
                let names: Vec<&str> = left_out.iter().map(String::as_str).collect();
                // alloc: the error's text, for an error hir found
                format!("{} ", stdx::list::list(names.iter(), Join::AndNoSerialComma, true))
            }
        };
        let and_other = if record.fields.is_empty() { "" } else { "...and other " };
        let (s, were) = if left_out.len() == 1 { ("", "was") } else { ("s", "were") };
        // alloc: the error's text, for an error hir found
        diag.note(format!("{and_other}private field{s} {names}that {were} not provided"));
    }
    let tail = record.span(cx, TextRange::new(record.path_range().end(), record.node.text_range().end()));
    let ty = record.ty(cx);
    let self_adt = ty.as_ref().and_then(|it| it.as_adt()).unwrap_or(adt);
    // alloc: the error's parts, for an error hir found
    let mut constructors: Vec<(bool, String, usize)> = Vec::new();
    if let Some(ty) = &ty {
        ty.iterate_assoc_items(cx.db, |item| {
            if let hir::AssocItem::Function(f) = item
                && !f.has_self_param(cx.db)
                && let hir::AssocItemContainer::Impl(it) = item.container(cx.db)
                && it.trait_(cx.db).is_none()
                && f.ret_type(cx.db).as_adt() == Some(self_adt)
            {
                // alloc: the error's text, for an error hir found
                let name = f.name(cx.db).as_str().to_owned();
                constructors.push((!name.starts_with("new"), name, f.assoc_fn_params(cx.db).len()));
            }
            None::<()>
        });
    }
    radix::sort_by_key(&mut constructors, |(later, _, _)| u8::from(*later));
    // alloc: the error's text, for an error hir found
    let call = |name: &str, args: usize| format!("::{name}({})", vec!["_"; args].join(", "));
    match constructors.as_slice() {
        [] => {}
        [(_, name, args)] => {
            diag.suggestion_verbose(
                // clone: the error's parts, for an error hir found
                tail.clone(),
                // alloc: the error's text, for an error hir found
                format!("you might have meant to use the `{name}` associated function"),
                call(name, *args),
                Applicability::MaybeIncorrect,
            );
        }
        many => {
            diag.suggestions(
                // clone: the error's parts, for an error hir found
                tail.clone(),
                "you might have meant to use an associated function to build this type",
                many.iter().map(|(_, name, args)| call(name, *args)),
                Applicability::MaybeIncorrect,
                SuggestionStyle::ShowAlways,
            );
        }
    }
    if let Some(ty) = &ty
        && let Some(default) = hir::Trait::lang(cx.db, cx.krate, LangItem::Default)
        && ty.impls_trait(cx.db, default, &[])
    {
        diag.multipart_suggestion(
            "consider using the `Default` trait",
            // alloc: the error's parts, for an error hir found
            vec![
                // alloc: the error's text, for an error hir found
                (path.shrink_to_lo(), "<".to_owned()),
                // alloc: the error's text, for an error hir found
                (tail, " as std::default::Default>::default()".to_owned()),
            ],
            Applicability::MaybeIncorrect,
        );
        // alloc: the error's text, for an error hir found
        diag.fact_opt(cx.implements_fact(default, ty), format!("whether `{adt_ty_text}` implements `Default`"));
    }
    diag
}

/// The struct pattern forms (`check_struct_pat_fields`).
fn pattern<'db>(cx: &Cx<'_, 'db>, record: &Record<'db>, variant: hir::Variant) -> Option<Diag> {
    let found = pat_fields(cx, record, variant);
    if !has_unmentioned_error(record, variant, &found) {
        return None;
    }
    if let Some(diag) = tuple_variant_as_struct(cx, record, variant) {
        return Some(diag);
    }
    let module = record.module(cx);
    let accessible: Vec<hir::Field> =
        // alloc: the error's parts, for an error hir found
        found.unmentioned.iter().copied().filter(|it| suggestable(cx, *it, module)).collect();
    Some(match accessible.is_empty() {
        true => no_accessible_fields(cx, record),
        false => unmentioned(cx, record, &accessible, accessible.len() != found.unmentioned.len()),
    })
}

/// `error_no_accessible_fields`.
fn no_accessible_fields(cx: &Cx<'_, '_>, record: &Record<'_>) -> Diag {
    let whole = record.span(cx, record.node.text_range());
    let mut diag = Diag::error(None, "pattern requires `..` due to inaccessible fields", whole);
    let message = "ignore the inaccessible and unused fields";
    match record.fields.last() {
        Some(last) => {
            let tail = TextRange::new(last.range.end(), record.node.text_range().end());
            let text = &record.node.text()[tail - record.node.text_range().start()];
            let trimmed = text.trim_start();
            let (at, code) = match trimmed.starts_with(',') {
                true => {
                    let comma_hi = TextSize::try_from(text.len() - trimmed.len() + 1).unwrap_or_default();
                    (record.span(cx, TextRange::empty(tail.start() + comma_hi)), " ..")
                }
                false => (record.span(cx, last.range).shrink_to_hi(), ", .."),
            };
            diag.suggestion_verbose(at, message, code, Applicability::MachineApplicable);
        }
        None => {
            let at = record.span(cx, TextRange::new(record.path_range().end(), record.node.text_range().end()));
            diag.suggestion_verbose(at, message, " { .. }", Applicability::MachineApplicable);
        }
    }
    diag
}

/// E0027 (`error_unmentioned_fields`).
fn unmentioned(cx: &Cx<'_, '_>, record: &Record<'_>, fields: &[hir::Field], inaccessible: bool) -> Diag {
    // alloc: the error's parts, for an error hir found
    let names: Vec<String> = fields.iter().map(|it| it.name(cx.db).as_str().to_owned()).collect();
    let and_inaccessible = if inaccessible { " and inaccessible fields" } else { "" };
    let field_names = match names.as_slice() {
        // alloc: the error's text, for an error hir found
        [only] => format!("field `{only}`{and_inaccessible}"),
        many => {
            // alloc: the error's text, for an error hir found
            let many: Vec<String> = many.iter().map(|it| format!("`{it}`")).collect();
            // alloc: the error's text, for an error hir found
            format!("fields {}{and_inaccessible}", many.join(", "))
        }
    };
    let whole = record.span(cx, record.node.text_range());
    // alloc: clone: the error's parts, for an error hir found
    let mut diag = Diag::error(Some("E0027"), format!("pattern does not mention {field_names}"), whole.clone());
    // alloc: the error's text, for an error hir found
    diag.span_label(whole, format!("missing {field_names}"));
    let end = record.node.text_range().end();
    let (prefix, postfix, at) = match record.fields.last() {
        None => (" { ", " }", TextRange::new(record.path_range().end(), end)),
        Some(last) => (", ", " }", TextRange::new(last.range.end(), end)),
    };
    let at = record.span(cx, at);
    let len = names.len();
    let rest = if inaccessible { ", .." } else { "" };
    let included: Vec<String> =
        // alloc: clone: the error's text, for an error hir found
        names.iter().map(|it| if is_number(it) { format!("{it}: _") } else { it.clone() }).collect();
    let s = if len == 1 { "" } else { "s" };
    let and_ignore = if inaccessible { " and ignore the inaccessible fields" } else { "" };
    diag.suggestion(
        // clone: the error's parts, for an error hir found
        at.clone(),
        // alloc: the error's text, for an error hir found
        format!("include the missing field{s} in the pattern{and_ignore}"),
        // alloc: the error's text, for an error hir found
        format!("{prefix}{}{rest}{postfix}", included.join(", ")),
        Applicability::MachineApplicable,
    );
    // alloc: the error's text, for an error hir found
    let ignored: Vec<String> = names.iter().map(|it| format!("{it}: _")).collect();
    let (these, them) = if len == 1 { ("this", "it") } else { ("these", "them") };
    diag.suggestion(
        // clone: the error's parts, for an error hir found
        at.clone(),
        // alloc: the error's text, for an error hir found
        format!("if you don't care about {these} missing field{s}, you can explicitly ignore {them}"),
        // alloc: the error's text, for an error hir found
        format!("{prefix}{}{rest}{postfix}", ignored.join(", ")),
        Applicability::MachineApplicable,
    );
    // alloc: the error's text, for an error hir found
    diag.suggestion(at, "or always ignore missing fields here", format!("{prefix}..{postfix}"), Applicability::MachineApplicable);
    diag
}
