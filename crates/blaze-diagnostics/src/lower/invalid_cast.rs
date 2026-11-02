//! An `as` rustc rejects, each `hir::CastError` as rustc words it: E0606 "casting `&u8` as
//! `u8` is invalid" (with the dereference or borrow to remove, the pointer or integer to cast
//! through, and why two pointers' metadata differ), E0054 "cannot cast `i32` as `bool`" (compare
//! with zero), E0604 "only `u8` can be cast as `char`" (`char::from_u32`), E0605
//! "non-primitive cast" (the borrow or `From` to use), E0607 a thin to a wide pointer, E0606 an
//! integer to a wide pointer, E0804 an auto trait added to a `dyn` through a pointer cast, E0641
//! a pointer of an unknown kind, a field-less enum with `Drop` cast to an integer, and E0606 of a
//! foreign enum with a non-exhaustive variant. The pointers' metadata and the added auto traits
//! are hir's, as the cast check found them.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/cast.rs` (`report_cast_error`,
//! `try_suggest_collection_to_bool`) and `diagnostics.rs` (`CannotCastToBool`, `IntToWide`,
//! `CastThinPointerToWidePointer`, `PtrCastAddAutoToObject`, `CastUnknownPointer`,
//! `CastEnumDrop`) (MIT or Apache-2.0).

use hir::{CastError, InFile};
use intern::{Symbol, sym};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use super::binary_operator_cannot_be_applied::{adt_def_span, code_fact};
use stdx::list::Join;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// The parts of the cast rustc's spans name: the whole `x as T`, `x` and `T`.
struct Spans {
    span: Span,
    expr: Span,
    cast: Span,
}

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidCast<'db>) -> Vec<Found> {
    let node = cx.node_view(d.expr);
    let file = node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let span = cx.span_of_ptr(d.expr);
    let cast = match node.value.left() {
        Some(ast::Expr::CastExpr(it)) => Some(it),
        _ => None,
    };
    let operand = cast.and_then(|it| it.expr());
    let spans = Spans {
        // clone: on the error path, for an error hir found
        expr: operand.map_or_else(|| span.clone(), |it| at(it.syntax().text_range())),
        cast: cast
            .and_then(|it| it.ty())
            // clone: on the error path, for an error hir found
            .map_or_else(|| span.clone(), |it| at(it.syntax().text_range())),
        span,
    };
    let e = cx.ty(&d.expr_ty);
    let c = cx.ty(&d.cast_ty);
    // alloc: the error's text, for an error hir found
    let invalid = || Diag::error(Some("E0606"), format!("casting `{e}` as `{c}` is invalid"), spans.span.clone());
    let operand_is_borrow = matches!(operand, Some(ast::Expr::RefExpr(_)));

    let mut diag = match d.error {
        CastError::NeedDeref => {
            let mut diag = invalid();
            match operand {
                Some(ast::Expr::RefExpr(borrow)) => {
                    let inner = peel_borrows(borrow);
                    let start = borrow.syntax().text_range().start();
                    diag.suggestion_verbose(
                        at(TextRange::new(start, inner.syntax().text_range().start())),
                        "remove the unneeded borrow",
                        "",
                        Applicability::MachineApplicable,
                    );
                }
                _ => {
                    diag.suggestion_verbose(
                        spans.expr.shrink_to_lo(),
                        "dereference the expression",
                        "*",
                        Applicability::MachineApplicable,
                    );
                }
            }
            diag
        }
        CastError::NeedViaPtr | CastError::NeedViaThinPtr => {
            let mut diag = invalid();
            if d.cast_ty.is_int_or_uint() {
                let enum_ = (!operand_is_borrow)
                    .then(|| d.expr_ty.as_reference())
                    .flatten()
                    .and_then(|(inner, _)| match inner.as_adt() {
                        Some(hir::Adt::Enum(it)) if payload_free(cx, it) => Some((it, inner)),
                        _ => None,
                    });
                if let Some((enum_, inner)) = enum_ {
                    diag.suggestion_verbose(
                        spans.expr.shrink_to_lo(),
                        "try dereferencing before the cast",
                        "*",
                        Applicability::MaybeIncorrect,
                    );
                    if !inner.is_copy(cx.db)
                        && let Some(def) = adt_def_span(cx, hir::Adt::Enum(enum_))
                    {
                        diag.suggestion_verbose(
                            def.shrink_to_lo(),
                            "add `#[derive(Copy, Clone)]` to the enum definition",
                            "#[derive(Copy, Clone)]\n",
                            Applicability::MaybeIncorrect,
                        );
                    }
                } else {
                    let through = match d.error {
                        CastError::NeedViaPtr => "a raw pointer",
                        _ => "a thin pointer",
                    };
                    // alloc: the error's text, for an error hir found
                    diag.help(format!("cast through {through} first"));
                }
            }
            collection_to_bool(cx, d, &spans, &mut diag);
            diag
        }
        CastError::NeedViaInt => {
            let mut diag = invalid();
            diag.help("cast through an integer first");
            diag
        }
        CastError::IllegalCast | CastError::Unknown => invalid(),
        CastError::DifferingKinds => {
            let mut diag = invalid();
            use hir::CastPointerKind::*;
            let note = match (d.expr_pointer, d.cast_pointer) {
                (Some(VTable(_)), Some(VTable(_))) => Some("the trait objects may have different vtables"),
                (Some(OfParam(_) | OfAlias), Some(OfParam(_) | OfAlias | VTable(_) | Length))
                | (Some(VTable(_) | Length), Some(OfParam(_) | OfAlias)) => {
                    Some("the pointers may have different metadata")
                }
                (Some(VTable(_)), Some(Length)) | (Some(Length), Some(VTable(_))) => {
                    Some("the pointers have different metadata")
                }
                _ => None,
            };
            if let Some(note) = note {
                diag.note(note);
            }
            diag
        }
        CastError::CastToBool => {
            // alloc: the error's text, for an error hir found
            let mut diag = Diag::error(Some("E0054"), format!("cannot cast `{e}` as `bool`"), spans.span.clone());
            if is_numeric(&d.expr_ty) {
                diag.suggestion_verbose(
                    spans.expr.shrink_to_hi().with_hi(spans.span.hi),
                    "compare with zero instead",
                    " != 0",
                    Applicability::MachineApplicable,
                );
            } else {
                // clone: on the error path, for an error hir found
                diag.span_label(spans.span.clone(), "unsupported cast");
            }
            diag
        }
        CastError::CastToChar => {
            let mut diag = Diag::error(
                Some("E0604"),
                // alloc: the error's text, for an error hir found
                format!("only `u8` can be cast as `char`, not `{e}`"),
                // clone: on the error path, for an error hir found
                spans.span.clone(),
            );
            // clone: on the error path, for an error hir found
            diag.span_label(spans.span.clone(), "invalid cast");
            if is_numeric(&d.expr_ty) {
                let builtin = d.expr_ty.as_builtin().map(|it| it.name());
                match builtin.as_ref().map(|it| it.as_str()) {
                    Some("u32") => {
                        diag.multipart_suggestion(
                            "consider using `char::from_u32` instead",
                            // alloc: the suggestion's parts, for an error hir found
                            vec![
                                // alloc: the suggestion's text, for an error hir found
                                (spans.expr.shrink_to_lo(), "char::from_u32(".to_owned()),
                                // alloc: the suggestion's text, for an error hir found
                                (spans.expr.shrink_to_hi().to(&spans.cast), ")".to_owned()),
                            ],
                            Applicability::MachineApplicable,
                        );
                    }
                    Some("i8") => {
                        // clone: on the error path, for an error hir found
                        diag.span_help(spans.span.clone(), "consider casting from `u8` instead");
                    }
                    _ => {
                        diag.span_help(
                            // clone: on the error path, for an error hir found
                            spans.span.clone(),
                            "consider using `char::from_u32` instead (via a `u32`)",
                        );
                    }
                }
            }
            diag
        }
        CastError::NonScalar => non_scalar(cx, d, &spans, operand, &e, &c),
        CastError::SizedUnsizedCast => Diag::error(
            Some("E0607"),
            // alloc: the error's text, for an error hir found
            format!("cannot cast thin pointer `{e}` to wide pointer `{c}`"),
            // clone: on the error path, for an error hir found
            spans.span.clone(),
        ),
        CastError::IntToWideCast => {
            let known = match d.cast_pointer {
                Some(hir::CastPointerKind::VTable(_)) => Some("a vtable"),
                Some(hir::CastPointerKind::Length) => Some("a length"),
                _ => None,
            };
            let wide = if known.is_some() { "is" } else { "may be" };
            let mut diag = Diag::error(
                Some("E0606"),
                // alloc: the error's text, for an error hir found
                format!("cannot cast `{e}` to a pointer that {wide} wide"),
                // clone: on the error path, for an error hir found
                spans.cast.clone(),
            );
            diag.span_label(
                // clone: on the error path, for an error hir found
                spans.cast.clone(),
                // alloc: the error's text, for an error hir found
                format!(
                    "creating a `{c}` requires both an address and {}",
                    known.unwrap_or("type-specific metadata")
                ),
            );
            diag.span_label(
                // clone: on the error path, for an error hir found
                spans.expr.clone(),
                "consider casting this expression to `*const ()`, then using `core::ptr::from_raw_parts`",
            );
            if let Some(hir::CastPointerKind::OfParam(param)) = d.cast_pointer
                && d.cast_ty.as_raw_ptr().is_some_and(|(pointee, _)| pointee.as_type_param(cx.db).is_some())
            {
                // alloc: the error's text, for an error hir found
                diag.note(format!(
                    "the type parameter `{}` is not known to be `Sized`, so this pointer may be wide",
                    param.name(cx.db).display(cx.db, cx.edition)
                ));
            }
            diag
        }
        CastError::PtrPtrAddingAutoTraits => {
            let mut traits: Vec<String> = d
                .added_auto_traits
                .iter()
                // alloc: the error's text, for an error hir found
                .map(|&it| format!("`{}`", cx.def_path(hir::ModuleDef::Trait(it))))
                // alloc: the error's text, for an error hir found
                .collect();
            traits.sort();
            let list = match traits.as_slice() {
                // alloc: the error's text, for an error hir found
                [one] => format!("auto trait {one}"),
                // alloc: the error's text, for an error hir found
                _ => format!("auto traits {}", stdx::list::list(traits.iter(), Join::And, false)),
            };
            let mut diag = Diag::error(
                Some("E0804"),
                // alloc: the error's text, for an error hir found
                format!("cannot add {list} to dyn bound via pointer cast"),
                // clone: on the error path, for an error hir found
                spans.span.clone(),
            );
            // clone: on the error path, for an error hir found
            diag.span_label(spans.span.clone(), "unsupported cast");
            diag.note("this could allow UB elsewhere");
            diag.help("use `transmute` if you're sure this is sound");
            diag
        }
        CastError::UnknownCastPtrKind => {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0641"), "cannot cast to a pointer of an unknown kind", spans.cast.clone());
            // clone: on the error path, for an error hir found
            diag.span_label(spans.cast.clone(), "needs more type information");
            diag.note("the type information given here is insufficient to check whether the pointer cast is valid");
            diag
        }
        CastError::UnknownExprPtrKind => {
            // clone: on the error path, for an error hir found
            let mut diag = Diag::error(Some("E0641"), "cannot cast from a pointer of an unknown kind", spans.cast.clone());
            diag.span_label(
                // clone: on the error path, for an error hir found
                spans.span.clone(),
                "the type information given here is insufficient to check whether the pointer cast is valid",
            );
            diag
        }
        CastError::CastEnumDrop => Diag::error(
            None,
            // alloc: the error's text, for an error hir found
            format!("cannot cast enum `{e}` into integer `{c}` because it implements `Drop`"),
            // clone: on the error path, for an error hir found
            spans.span.clone(),
        ),
        CastError::ForeignNonExhaustiveAdt => {
            let mut diag = invalid();
            diag.note("cannot cast an enum with a non-exhaustive variant when it's defined in another crate");
            diag
        }
    };
    code_fact(cx, d.expr.map(|it| it.syntax_node_ptr()), &mut diag);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Bodies)]
}

/// rustc's `CastError::NonScalar`: E0605, with the borrow or `From` to use instead.
fn non_scalar<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::InvalidCast<'db>,
    spans: &Spans,
    operand: Option<ast::Expr<'_>>,
    e: &str,
    c: &str,
) -> Diag {
    // alloc: the error's text, for an error hir found
    let mut diag = Diag::error(Some("E0605"), format!("non-primitive cast: `{e}` as `{c}`"), spans.span.clone());
    if let Some(ast::Expr::RefExpr(_)) = operand
        && let Some(snippet) = cx.snippet(&spans.expr)
    {
        // alloc: the error's text, for an error hir found
        diag.note(format!("casting reference expression `{snippet}` because `&` binds tighter than `as`"));
    }

    let prefix = |it: hir::Mutability| match it {
        hir::Mutability::Mut => "mut ",
        hir::Mutability::Shared => "",
    };
    let coerces = |from: &hir::Type<'db>, mutability| {
        from.add_reference(cx.db, mutability).could_coerce_to(cx.db, &d.cast_ty)
    };
    let mut sugg: Option<(String, bool)> = None;
    let mut sugg_mutref = false;
    if let Some((cast_inner, mutability)) = d.cast_ty.as_reference() {
        if let Some((pointee, _)) = d.expr_ty.as_raw_ptr()
            && coerces(&pointee, mutability)
        {
            // alloc: the suggestion's text, for an error hir found
            sugg = Some((format!("&{}*", prefix(mutability)), cx.ty(&cast_inner) == cx.ty(&pointee)));
        } else if let Some((expr_inner, hir::Mutability::Shared)) = d.expr_ty.as_reference()
            && mutability == hir::Mutability::Mut
            && coerces(&expr_inner, hir::Mutability::Mut)
        {
            sugg_mutref = true;
        }
        if !sugg_mutref && sugg.is_none() && coerces(&d.expr_ty, mutability) {
            // alloc: the suggestion's text, for an error hir found
            sugg = Some((format!("&{}", prefix(mutability)), false));
        }
    } else if let Some((_, mutability)) = d.cast_ty.as_raw_ptr()
        && coerces(&d.expr_ty, mutability)
    {
        // alloc: the suggestion's text, for an error hir found
        sugg = Some((format!("&{}", prefix(mutability)), false));
    }

    if sugg_mutref {
        // clone: on the error path, for an error hir found
        diag.span_label(spans.span.clone(), "invalid cast");
        // clone: on the error path, for an error hir found
        diag.span_note(spans.expr.clone(), "this reference is immutable");
        // clone: on the error path, for an error hir found
        diag.span_note(spans.cast.clone(), "trying to cast to a mutable reference type");
    } else if let Some((code, remove_cast)) = sugg {
        // clone: on the error path, for an error hir found
        diag.span_label(spans.span.clone(), "invalid cast");
        let has_parens = cx.snippet(&spans.expr).is_some_and(|it| it.starts_with('('));
        let needs_parens = !has_parens && matches!(operand, Some(ast::Expr::CastExpr(_)));
        // alloc: the suggestion's parts, for an error hir found
        let mut parts = vec![(spans.expr.shrink_to_lo(), code)];
        if needs_parens {
            parts[0].1 += "(";
            // alloc: the suggestion's text, for an error hir found
            parts.push((spans.expr.shrink_to_hi(), ")".to_owned()));
        }
        if remove_cast {
            // alloc: empty, allocates nothing
            parts.push((spans.expr.shrink_to_hi().to(&spans.cast), String::new()));
        }
        diag.multipart_suggestion("consider borrowing the value", parts, Applicability::MachineApplicable);
    } else if !(d.cast_ty.is_fn() || d.cast_ty.is_closure()) {
        if let Some(from) = core_trait(cx, sym::convert, sym::From)
            && d.cast_ty.impls_trait(cx.db, from, std::slice::from_ref(&d.expr_ty))
        {
            let to = match d.cast_ty.as_adt() {
                Some(adt) => {
                    // alloc: the suggestion's text, for an error hir found
                    let args: Vec<String> = d.cast_ty.type_arguments().map(|it| cx.ty(&it)).collect();
                    let path = cx.def_path(hir::ModuleDef::Adt(adt));
                    // alloc: the suggestion's text, for an error hir found
                    if args.is_empty() { path } else { format!("{path}::<{}>", args.join(", ")) }
                }
                // alloc: the suggestion's text, for an error hir found
                None => c.to_owned(),
            };
            diag.multipart_suggestion(
                "consider using the `From` trait instead",
                // alloc: the suggestion's parts, for an error hir found
                vec![
                    // alloc: the suggestion's text, for an error hir found
                    (spans.expr.shrink_to_lo(), format!("{to}::from(")),
                    // alloc: the suggestion's text, for an error hir found
                    (spans.expr.shrink_to_hi().to(&spans.cast), ")".to_owned()),
                ],
                Applicability::MaybeIncorrect,
            );
        }
        if matches!(d.expr_ty.as_adt(), Some(hir::Adt::Enum(_))) && is_numeric(&d.cast_ty) {
            diag.span_label(
                // clone: on the error path, for an error hir found
                spans.span.clone(),
                "an `as` expression can be used to convert enum types to numeric types only if the enum type is unit-only or field-less",
            );
            diag.note("see https://doc.rust-lang.org/reference/items/enumerations.html#casting for more information");
        } else {
            diag.span_label(
                // clone: on the error path, for an error hir found
                spans.span.clone(),
                "an `as` expression can only be used to convert between primitive types or to coerce to a specific trait object",
            );
        }
    } else {
        // clone: on the error path, for an error hir found
        diag.span_label(spans.span.clone(), "invalid cast");
    }
    collection_to_bool(cx, d, spans, &mut diag);
    diag
}

/// rustc's `try_suggest_collection_to_bool`: `s as bool` of a string or a slice.
fn collection_to_bool<'db>(cx: &Cx<'_, 'db>, d: &hir::InvalidCast<'db>, spans: &Spans, diag: &mut Diag) {
    if !d.cast_ty.is_bool() {
        return;
    }
    let Some(deref) = d.expr_ty.autoderef(cx.db).find(|it| it.is_str() || it.is_slice()) else { return };
    let deref_ty = cx.ty(&deref);
    if deref_ty != cx.ty(&d.expr_ty.strip_references()) {
        diag.span_note(
            // clone: on the error path, for an error hir found
            spans.expr.clone(),
            // alloc: the error's text, for an error hir found
            format!("this expression `Deref`s to `{deref_ty}` which implements `is_empty`"),
        );
    }
    diag.multipart_suggestion(
        // alloc: the suggestion's text, for an error hir found
        format!(
            "consider using the `is_empty` method on `{}` to determine if it contains anything",
            cx.ty(&d.expr_ty)
        ),
        // alloc: the suggestion's parts, for an error hir found
        vec![
            // alloc: the suggestion's text, for an error hir found
            (spans.expr.shrink_to_lo(), "!".to_owned()),
            // alloc: the suggestion's text, for an error hir found
            (spans.span.with_lo(spans.expr.hi), ".is_empty()".to_owned()),
        ],
        Applicability::MaybeIncorrect,
    );
}

/// rustc's `Expr::peel_borrows`.
fn peel_borrows(borrow: ast::RefExpr<'_>) -> ast::Expr<'_> {
    let mut inner = ast::Expr::RefExpr(borrow);
    while let ast::Expr::RefExpr(it) = inner
        && let Some(next) = it.expr()
    {
        inner = next;
    }
    inner
}

/// rustc's `AdtDef::is_payloadfree`: no variant has fields.
fn payload_free(cx: &Cx<'_, '_>, enum_: hir::Enum) -> bool {
    enum_.variants_iter(cx.db).all(|it| it.fields(cx.db).is_empty())
}

fn is_numeric(ty: &hir::Type<'_>) -> bool {
    ty.is_int_or_uint() || ty.is_float()
}

/// The trait `core::{module}::{name}`, as rustc's diagnostic items find it: core is the crate of
/// the `Sized` lang item, and the path is two hashed scope lookups of interned names.
fn core_trait(cx: &Cx<'_, '_>, module: Symbol, name: Symbol) -> Option<hir::Trait> {
    let sized = hir_def::lang_item::lang_items(cx.db, cx.krate.base()).Sized?;
    let core = hir::Trait::from(sized).module(cx.db).krate(cx.db);
    let path = [hir::Name::new_symbol_root(module), hir::Name::new_symbol_root(name)];
    core.root_module(cx.db).resolve_mod_path(cx.db, path)?.find_map(|it| match it {
        hir::ItemInNs::Types(hir::ModuleDef::Trait(it)) => Some(it),
        _ => None,
    })
}
