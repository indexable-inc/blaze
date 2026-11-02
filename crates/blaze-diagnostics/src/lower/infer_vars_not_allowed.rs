//! E0121 "the placeholder `_` is not allowed within types on item signatures for functions":
//! `_` in a signature. In a function's return type and in a const's or a static's type, the
//! placeholders of that type are one error, with the type the body gives as the suggestion
//! (or an `impl Trait` of it, or the note that it cannot be named); elsewhere each is its own.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/collect.rs` (`bad_placeholder`,
//! `ItemCtxt::ty_infer`, `report_placeholder_type_error`, `recover_infer_ret_ty`,
//! `suggest_impl_trait`, `HirPlaceholderCollector`), `compiler/rustc_hir_analysis/src/collect/
//! type_of.rs` (`type_of`, `infer_placeholder_type`) and `compiler/rustc_hir_analysis/src/
//! diagnostics.rs` (`PlaceholderNotAllowedItemSignatures`) (MIT or Apache-2.0).

use hir::{CallableKind, InFile, LangItem};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::{
    cx::Cx,
    lower::{Found, Phase, incorrect_generics_len::site_fact},
    model::{Applicability, Diag},
};

/// The placeholders under `node` in source order (a preorder walk; placeholders don't nest):
/// rustc's `HirPlaceholderCollector`, of `_` types and of `_` const arguments and array lengths.
fn placeholders(node: ast::Node<'_>) -> impl Iterator<Item = TextRange> {
    node.descendants()
        .filter(|it| {
            ast::InferType::can_cast(it.kind())
                || (ast::UnderscoreExpr::can_cast(it.kind())
                    && it.parent().is_some_and(|p| ast::ConstArg::can_cast(p.kind())))
        })
        .map(|it| it.text_range())
}

/// rustc's `bad_placeholder`: `kind` pluralized, the spans sorted, each labelled.
fn bad_placeholder(cx: &Cx<'_, '_>, file: hir::HirFileId, spans: &[TextRange], kind: &str) -> Diag {
    let kind = match kind.ends_with('s') {
        // alloc: the error's text, for an error hir found
        true => format!("{kind}es"),
        // alloc: the error's text, for an error hir found
        false => format!("{kind}s"),
    };
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let mut diag = Diag::error(
        Some("E0121"),
        // alloc: the error's text, for an error hir found
        format!("the placeholder `_` is not allowed within types on item signatures for {kind}"),
        span(spans[0]),
    );
    for range in &spans[1..] {
        diag.primary(span(*range));
    }
    for range in spans {
        diag.span_label(span(*range), "not allowed in type signatures");
    }
    diag
}

fn in_assoc(item: ast::Item<'_>) -> bool {
    item.syntax().parent().is_some_and(|it| ast::AssocItemList::can_cast(it.kind()))
}

/// rustc's `def_descr` of the item a signature is of.
fn item_descr(item: ast::Item<'_>) -> &'static str {
    match item {
        ast::Item::Fn(it) if in_assoc(item) => {
            match it.param_list().and_then(|it| it.self_param()).is_some() {
                true => "method",
                false => "associated function",
            }
        }
        ast::Item::Fn(_) => "function",
        ast::Item::Struct(_) => "struct",
        ast::Item::Enum(_) => "enum",
        ast::Item::Union(_) => "union",
        ast::Item::Trait(_) => "trait",
        ast::Item::TypeAlias(_) if in_assoc(item) => "associated type",
        ast::Item::TypeAlias(_) => "type alias",
        ast::Item::Impl(_) => "implementation",
        ast::Item::Const(_) if in_assoc(item) => "associated constant",
        ast::Item::Const(_) => "constant",
        ast::Item::Static(_) => "static",
        _ => "item",
    }
}

/// Whether rustc's `make_suggestable` can write `ty`: no closure, function item, opaque type
/// or error in it.
fn suggestable(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> bool {
    let mut ok = true;
    ty.walk(cx.db, |it| {
        let fn_item = it.as_callable(cx.db).is_some_and(|c| {
            matches!(
                c.kind(),
                CallableKind::Function(_)
                    | CallableKind::TupleStruct(_)
                    | CallableKind::TupleEnumVariant(_)
            )
        });
        if it.is_unknown() || it.is_closure() || fn_item || it.as_impl_traits(cx.db).is_some() {
            ok = false;
        }
    });
    ok
}

/// rustc's `suggest_impl_trait`: `impl Iterator<Item = T>`, `impl Future<Output = T>` or
/// `impl Fn(A) -> B` for a type that cannot be named.
fn impl_trait<'db>(cx: &Cx<'_, 'db>, ty: &hir::Type<'db>) -> Option<String> {
    let db = cx.db;
    let assoc = |lang: LangItem, assoc: &str| -> Option<String> {
        let trait_ = hir::Trait::lang(db, cx.krate, lang)?;
        if !ty.impls_trait(db, trait_, &[]) {
            return None;
        }
        let alias = trait_.items_iter(db).filter_map(|it| it.as_type_alias()).find(|it| {
            it.name(db).as_str() == assoc
        })?;
        let item = ty.normalize_trait_assoc_type(db, &[], alias)?;
        // alloc: the suggestion's text, for an error hir found
        Some(format!(
            "impl {}<{assoc} = {}>",
            trait_.name(db).display(db, cx.edition),
            cx.ty(&item)
        ))
    };
    if let Some(it) = assoc(LangItem::Iterator, "Item") {
        return Some(it);
    }
    if let Some(it) = assoc(LangItem::Future, "Output") {
        return Some(it);
    }
    let callable = ty.as_callable(db)?;
    let trait_name = match callable.kind() {
        CallableKind::Closure(closure) => closure.fn_trait(db).as_str(),
        CallableKind::Function(_) | CallableKind::TupleStruct(_) | CallableKind::TupleEnumVariant(_) => "Fn",
        _ => return None,
    };
    // alloc: the suggestion's text, for an error hir found
    let mut out = format!("impl {trait_name}(");
    for (i, param) in callable.params().iter().enumerate() {
        if !suggestable(cx, param.ty()) {
            return None;
        }
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&cx.ty(param.ty()));
    }
    out.push(')');
    let ret = callable.return_type();
    if !ret.is_unit() {
        out.push_str(" -> ");
        out.push_str(&cx.ty(&ret));
    }
    Some(out)
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::InferVarsNotAllowed) -> Vec<Found> {
    let node = cx.syntax_view(d.node);
    let file = node.file_id;
    let at = node.value.text_range();
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let Some(item) = node.value.ancestors().find_map(ast::Item::cast) else {
        let mut diag = bad_placeholder(cx, file, &[at], "item");
        site_fact(cx, &mut diag, node);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Items)];
    };

    // The type of a const or a static with a body, and a function's return type, are
    // inferred from the body and their placeholders reported together.
    let grouped = match item {
        ast::Item::Const(it) => it.ty().zip(it.body()).map(|(ty, body)| {
            let kind = if in_assoc(item) { "associated constant" } else { "constant" };
            (ty.syntax(), body, kind, false)
        }),
        ast::Item::Static(it) => {
            it.ty().zip(it.body()).map(|(ty, body)| (ty.syntax(), body, "static variable", false))
        }
        ast::Item::Fn(it) => it.ret_type().and_then(|ret| ret.ty()).zip(it.body()).map(|(ty, body)| {
            (ty.syntax(), ast::Expr::BlockExpr(body), "return type", true)
        }),
        _ => None,
    }
    .filter(|(ty, ..)| ty.text_range().contains_range(at));

    let Some((ty, body, kind, is_return)) = grouped else {
        let in_variant = node.value.ancestors().take_while(|it| *it != item.syntax()).any(|it| {
            ast::Variant::can_cast(it.kind())
        });
        let kind = if in_variant { "variant" } else { item_descr(item) };
        let mut diag = bad_placeholder(cx, file, &[at], kind);
        site_fact(cx, &mut diag, node);
        // alloc: the error's parts, for an error hir found
        return vec![Found::new(diag, Phase::Items)];
    };
    // Every placeholder of the type has its own hir diagnostic; only the first one's builds the
    // error, and the others stop at the first node of the walk.
    if placeholders(ty).next().is_some_and(|first| first != at) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: the error's spans, for an error hir found
    let mut spans: Vec<TextRange> = placeholders(ty).collect();
    if spans.is_empty() {
        spans.push(at);
    }
    let mut diag = bad_placeholder(cx, file, &spans, kind);
    let inferred = cx.vsema.type_of_expr(body).map(|it| it.original());
    let ty_span = span(ty.text_range());
    match (is_return, inferred) {
        (_, None) => {}
        (_, Some(inferred)) if inferred.is_unknown() || inferred.contains_unknown() => {}
        (true, Some(inferred)) => {
            if suggestable(cx, &inferred) {
                diag.suggestion_verbose(
                    ty_span,
                    "replace with the correct return type",
                    cx.ty(&inferred),
                    Applicability::MachineApplicable,
                );
            } else if let Some(sugg) = impl_trait(cx, &inferred) {
                diag.suggestion_verbose(
                    ty_span,
                    "replace with an appropriate return type",
                    sugg,
                    Applicability::MachineApplicable,
                );
            } else if inferred.is_closure() {
                diag.help("consider using an `Fn`, `FnMut`, or `FnOnce` trait bound");
            }
            if inferred.is_closure() {
                diag.note(
                    "for more information on `Fn` traits and closure types, see \
                     https://doc.rust-lang.org/book/ch13-01-closures.html",
                );
            }
        }
        (false, Some(inferred)) => {
            if suggestable(cx, &inferred) {
                diag.suggestion_verbose(
                    ty_span,
                    "replace this with a fully-specified type",
                    cx.ty(&inferred),
                    Applicability::MachineApplicable,
                );
            } else {
                diag.span_note(
                    span(body.syntax().text_range()),
                    // alloc: the error's text, for an error hir found
                    format!("however, the inferred type `{}` cannot be named", cx.ty(&inferred)),
                );
            }
        }
    }
    site_fact(cx, &mut diag, node);
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}
