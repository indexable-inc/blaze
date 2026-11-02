//! The notes rustc adds to a trait solver error for why the bound was required: rustc's
//! `note_obligation_cause_code` for the causes the front end records ("required by a bound in
//! `f`" on the bound `GenericDef::bound_source` maps the cause's `bound_index` to (a projection
//! goal's on its `Assoc = ..`), "the implicit `Sized` requirement on this type parameter",
//! "required by this const generic parameter", "consider relaxing the implicit `Sized`
//! restriction", "all function arguments must have a statically known size" and the other
//! `Sized` notes). The causes of a type mismatch (`if` and `match` arms, a return value, a
//! block's tail) add nothing here; `type_mismatch.rs` gives their labels, as rustc's
//! `note_type_err` does.
//!
//! Follows rustc's `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (`note_obligation_cause_code`) (MIT or Apache-2.0).
//!
//! Not yet: the argument or `let` a `Sized` bound is of, for rustc's "consider borrowing here";
//! rustc's "sealed trait" note and help for a trait's bound on a trait that is not reachable;
//! the name of an `impl` whose bound it is; `short_string`'s shortened item name.
//!
//! Cost: nothing until the solver reports the error; then one `predicate_sources` of the item.

use blaze_items::{Fact, ItemId};
use either::Either;
use hir::{AsAssocItem as _, InFile, LangItem, ModuleDef, PathResolution};
use hir_def::{FunctionId, unstable_features::UnstableFeatures};
use intern::sym;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::Cx,
    lower::{
        unimplemented_trait::{constraint_range, is_lang, overlaps, same_line},
        unresolved_ident::{Ns, def_span},
    },
    model::{Applicability, Diag, MultiSpan, Span},
    print,
};

/// rustc's `note_obligation_cause_code` of `cause`, for a bound of `trait_` when the failed
/// goal is a trait's.
pub(super) fn note(cx: &Cx<'_, '_>, diag: &mut Diag, cause: hir::ObligationCauseCode, trait_: Option<hir::Trait>) {
    note_of(cx, diag, cause, trait_, None);
}

/// `note` for a projection goal on `assoc` of `trait_`: a written bound's span is its
/// `Assoc = ..` constraint, as rustc's lowering gives the projection predicate.
pub(super) fn note_projection(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    cause: hir::ObligationCauseCode,
    trait_: hir::Trait,
    assoc: hir::TypeAlias,
) {
    note_of(cx, diag, cause, Some(trait_), Some(assoc));
}

fn note_of(
    cx: &Cx<'_, '_>,
    diag: &mut Diag,
    cause: hir::ObligationCauseCode,
    trait_: Option<hir::Trait>,
    assoc: Option<hir::TypeAlias>,
) {
    match cause {
        hir::ObligationCauseCode::WhereClauseInExpr { item, bound_index, .. } => {
            where_clause(cx, diag, item, bound_index, trait_, assoc)
        }
        hir::ObligationCauseCode::SizedArgumentType => {
            diag.note("all function arguments must have a statically known size");
            if !UnstableFeatures::query(cx.db, cx.krate.base()).is_enabled(&sym::unsized_fn_params) {
                diag.help("unsized fn params are gated as an unstable feature");
            }
        }
        hir::ObligationCauseCode::SizedReturnType => {
            diag.note("the return type of a function must have a statically known size");
        }
        hir::ObligationCauseCode::LetType => {
            diag.note("all local variables must have a statically known size");
        }
        hir::ObligationCauseCode::Misc
        | hir::ObligationCauseCode::BlockTailExpression
        | hir::ObligationCauseCode::ReturnValue
        | hir::ObligationCauseCode::IfExpression
        | hir::ObligationCauseCode::MatchExpressionArm { .. }
        | hir::ObligationCauseCode::IfExpressionWithNoElse
        | hir::ObligationCauseCode::ReturnNoExpression => {}
    }
}

/// rustc's "required by a bound in `item`" (`WhereClauseInExpr`): on the bound `bound_index`
/// is of, with "the implicit `Sized` requirement on this type parameter" for a parameter's
/// implicit `Sized` and "required by this const generic parameter" for a const parameter's
/// type; nothing for a trait's own `Self: Trait`, whose span rustc keeps dummy; on the item's
/// head for a bound with no syntax.
fn where_clause<'db>(
    cx: &Cx<'_, 'db>,
    diag: &mut Diag,
    item: hir::GenericDef,
    bound_index: u32,
    trait_: Option<hir::Trait>,
    assoc: Option<hir::TypeAlias>,
) {
    let (def, ns) = match item {
        hir::GenericDef::Function(it) => (ModuleDef::Function(it), Ns::Value),
        hir::GenericDef::Adt(it) => (ModuleDef::Adt(it), Ns::Type),
        hir::GenericDef::Trait(it) => (ModuleDef::Trait(it), Ns::Type),
        hir::GenericDef::TypeAlias(it) => (ModuleDef::TypeAlias(it), Ns::Type),
        hir::GenericDef::Const(it) => (ModuleDef::Const(it), Ns::Value),
        hir::GenericDef::Static(it) => (ModuleDef::Static(it), Ns::Value),
        hir::GenericDef::Impl(_) => {
            diag.note("blaze cannot name the `impl` whose bound this is required by");
            return;
        }
    };
    let name = print::def_path(cx, def);
    let source = item.bound_source(cx.db, bound_index);
    let (at, descr, label) = match source {
        Some(hir::BoundSource::ImpliedTrait(_)) => return,
        Some(hir::BoundSource::Written(ptr)) => {
            let at = match assoc {
                None => cx.span_of_ptr(ptr),
                Some(assoc) => {
                    let bound = cx.node_view(ptr);
                    let Some(range) = constraint_range(cx, bound.value, assoc) else {
                        // alloc: the error's text, for an error hir found
                        diag.note(format!(
                            "blaze cannot find the `{}` constraint in the bound of `{name}` this is required by",
                            assoc.name(cx.db).as_str(),
                        ));
                        return;
                    };
                    cx.span(bound.with_value(range))
                }
            };
            relax_sized(cx, diag, item, trait_);
            (Some(at), "a bound", "this bound")
        }
        Some(hir::BoundSource::ImplicitSized(param)) => {
            relax_sized(cx, diag, item, trait_);
            let at = param_span(cx, param.merge());
            if param.parent(cx.db) == item {
                (at, "an implicit `Sized` bound", "the implicit `Sized` requirement on this type parameter")
            } else {
                (at, "a bound", "this bound")
            }
        }
        Some(hir::BoundSource::ConstParamType(param)) => {
            (param_span(cx, param.merge()), "a const generic parameter", "this const generic parameter")
        }
        None => (None, "a bound", "this bound"),
    };
    // alloc: the error's text, for an error hir found
    let descr = format!("required by {descr} in `{name}`");
    let Some(at) = at else {
        match def_span(cx, ns, def) {
            Some(at) => diag.span_note(at, descr),
            None => diag.note(descr),
        };
        if let Some(hir::BoundSource::ImplicitSized(param)) = source {
            suggest_unsized(cx, diag, item, param);
        }
        return;
    };
    // clone: on the error path, for an error hir found
    let mut span = MultiSpan::of(at.clone());
    if let Some((ident, kind)) = ident_of(cx, item)
        && !overlaps(&ident, &at)
        && !same_line(cx, &ident, &at)
    {
        // alloc: the error's text, for an error hir found
        span.labels.push((ident, format!("required by a bound in this {kind}")));
    }
    // alloc: the error's text, for an error hir found
    span.labels.push((at, format!("required by {label} in `{name}`")));
    diag.span_note(span, descr);
    if let hir::GenericDef::Function(f) = item
        && let Ok(id) = FunctionId::try_from(f)
    {
        // alloc: the error's text, for an error hir found
        diag.fact(Fact::Signature(cx.key(ItemId::Function(id))), format!("the signature of `{name}`"));
    }
    if let Some(hir::BoundSource::ImplicitSized(param)) = source {
        suggest_unsized(cx, diag, item, param);
    }
}

/// rustc's `suggest_unsized_bound_if_applicable`, for the implicit `Sized` of `param` required
/// by `item`: when `param` is the local item's own and has no `Sized` bound written, the help
/// to use it through indirection where a struct, enum or union holds it bare, else "consider
/// relaxing the implicit `Sized` restriction" (`T: ?Sized`).
fn suggest_unsized(cx: &Cx<'_, '_>, diag: &mut Diag, item: hir::GenericDef, param: hir::TypeParam) {
    if param.parent(cx.db) != item || param.module(cx.db).krate(cx.db) != cx.krate {
        return;
    }
    let Some(src) = cx.vsema.source(param.merge()) else { return };
    let Either::Left(ast::TypeOrConstParam::Type(tp)) = src.value else { return };
    let Some(name) = tp.name() else { return };
    let Some(owner) = tp.syntax().parent().and_then(|list| list.parent()) else { return };
    let name = name.text();
    let wheres = owner.children().find_map(ast::WhereClause::cast);
    let is_param = |ty: Option<ast::Type<'_>>| match ty {
        Some(ast::Type::PathType(ty)) => is_bare_param(ty, &name),
        _ => false,
    };
    let preds: Vec<ast::TypeBoundList<'_>> = tp
        .type_bound_list()
        .into_iter()
        .chain(
            wheres
                .into_iter()
                .flat_map(|it| it.predicates())
                .filter(|pred| is_param(pred.ty()))
                .filter_map(|pred| pred.type_bound_list()),
        )
        // alloc: the error's parts, for an error hir found
        .collect();
    if preds.iter().flat_map(|it| it.bounds()).any(|it| bound_trait(cx, it).is_some_and(|t| is_lang(cx, t, LangItem::Sized))) {
        return;
    }
    if let hir::GenericDef::Adt(_) = item
        && let Some(at) = param_span(cx, param.merge())
    {
        // alloc: the error's parts, for an error hir found
        let mut bare = Vec::new();
        bare_uses(owner, &name, false, &mut bare);
        if !bare.is_empty() {
            // clone: on the error path, for an error hir found
            let mut span = MultiSpan::of(at.clone());
            // alloc: the error's text, for an error hir found
            span.labels.push((at, format!("this could be changed to `{name}: ?Sized`...")));
            for range in bare {
                // alloc: the error's text, for an error hir found
                span.labels.push((cx.span(src.with_value(range)), format!("...if indirection were used here: `Box<{name}>`")));
            }
            diag.span_help(
                span,
                // alloc: the error's text, for an error hir found
                format!(
                    "you could relax the implicit `Sized` bound on `{name}` if it were used through indirection like `&{name}` or `Box<{name}>`"
                ),
            );
            return;
        }
    }
    let last = if src.file_id.is_macro() {
        None
    } else {
        preds.iter().find_map(|list| list.bounds().last())
    };
    let (end, separator, open) = match last {
        Some(bound) => match dyn_return(bound) {
            Some(ret) => (ret.end(), " +", Some(ret.start())),
            None => (bound.syntax().text_range().end(), " +", None),
        },
        None => (tp.name().map_or(tp.syntax().text_range(), |it| it.syntax().text_range()).end(), ":", None),
    };
    let point = |offset| cx.span(src.with_value(TextRange::empty(offset)));
    // alloc: the suggestion's parts, for an error hir found
    let parts = match open {
        // alloc: the suggestion's text, for an error hir found
        Some(open) => vec![(point(open), "(".to_owned()), (point(end), format!("){separator} ?Sized"))],
        // alloc: the suggestion's text, for an error hir found
        None => vec![(point(end), format!("{separator} ?Sized"))],
    };
    diag.multipart_suggestion("consider relaxing the implicit `Sized` restriction", parts, Applicability::MachineApplicable);
}

/// Whether `ty` is the type parameter `name` written alone (rustc's one-segment resolved path).
fn is_bare_param(ty: ast::PathType<'_>, name: &str) -> bool {
    let Some(path) = ty.path() else { return false };
    path.qualifier().is_none()
        && path.segment().is_some_and(|it| {
            it.generic_arg_list().is_none()
                && it.parenthesized_arg_list().is_none()
                && it.name_ref().is_some_and(|it| it.text() == name)
        })
}

/// rustc's `FindTypeParam`: the types in `node` (where clauses skipped) that are the parameter
/// `name` alone, not behind a reference, a pointer, a trait object or another path.
fn bare_uses(node: ast::Node<'_>, name: &str, nested: bool, out: &mut Vec<TextRange>) {
    for child in node.children() {
        if ast::WhereClause::cast(child).is_some() {
            continue;
        }
        match ast::Type::cast(child) {
            Some(ast::Type::RefType(_) | ast::Type::PtrType(_) | ast::Type::DynTraitType(_)) => {}
            Some(ast::Type::PathType(ty)) if is_bare_param(ty, name) => {
                if !nested {
                    out.push(child.text_range());
                }
            }
            Some(ast::Type::PathType(_)) => bare_uses(child, name, true, out),
            _ => bare_uses(child, name, nested, out),
        }
    }
}

/// The `dyn Trait` a `Fn(..) -> &dyn Trait` bound returns (references peeled), which rustc
/// parenthesizes before adding `+ ?Sized`.
fn dyn_return(bound: ast::TypeBound<'_>) -> Option<TextRange> {
    let ast::Type::PathType(ty) = bound.ty()? else { return None };
    let segment = ty.path()?.segment()?;
    segment.parenthesized_arg_list()?;
    let mut ret = segment.ret_type()?.ty()?;
    while let ast::Type::RefType(it) = ret {
        ret = it.ty()?;
    }
    match ret {
        ast::Type::DynTraitType(it) if it.dyn_token().is_some() => Some(it.syntax().text_range()),
        _ => None,
    }
}

/// The span rustc gives the predicate `source` is of: the written bound; a type parameter's
/// name (to its default) for its implicit `Sized`; a const parameter for its type; `None` for a
/// trait's own `Self: Trait`, whose span is dummy.
pub(super) fn bound_span(cx: &Cx<'_, '_>, source: hir::BoundSource) -> Option<Span> {
    match source {
        hir::BoundSource::Written(ptr) => Some(cx.span_of_ptr(ptr)),
        hir::BoundSource::ImplicitSized(param) => param_span(cx, param.merge()),
        hir::BoundSource::ConstParamType(param) => param_span(cx, param.merge()),
        hir::BoundSource::ImpliedTrait(_) => None,
    }
}

/// rustc's span of a generic parameter: a type parameter's name (to its default), a const
/// parameter from `const` to its type (or default).
fn param_span(cx: &Cx<'_, '_>, param: hir::TypeOrConstParam) -> Option<Span> {
    let src = cx.vsema.source(param)?;
    let range = match src.value {
        Either::Left(ast::TypeOrConstParam::Type(it)) => {
            let name = it.name()?.syntax().text_range();
            match it.default_type() {
                Some(ty) => name.cover(ty.syntax().text_range()),
                None => name,
            }
        }
        Either::Left(ast::TypeOrConstParam::Const(it)) => {
            let end = match it.default_val() {
                Some(default) => default.syntax().text_range().end(),
                None => it.ty()?.syntax().text_range().end(),
            };
            let start = it.const_token().map_or(it.syntax().text_range().start(), |it| it.text_range().start());
            TextRange::new(start, end)
        }
        Either::Right(_) => return None,
    };
    Some(cx.span(src.with_value(range)))
}

/// rustc's `opt_item_ident` of `item` with its `DefKind::descr`, for "required by a bound in
/// this function".
fn ident_of(cx: &Cx<'_, '_>, item: hir::GenericDef) -> Option<(Span, &'static str)> {
    let db = cx.db;
    let (node, kind): (InFile<ast::Node<'_>>, _) = match item {
        hir::GenericDef::Function(it) => {
            let kind = if it.has_self_param(db) {
                "method"
            } else if it.as_assoc_item(db).is_some() {
                "associated function"
            } else {
                "function"
            };
            (cx.vsema.source(it)?.map(|it| it.syntax()), kind)
        }
        hir::GenericDef::Adt(hir::Adt::Struct(it)) => (cx.vsema.source(it)?.map(|it| it.syntax()), "struct"),
        hir::GenericDef::Adt(hir::Adt::Enum(it)) => (cx.vsema.source(it)?.map(|it| it.syntax()), "enum"),
        hir::GenericDef::Adt(hir::Adt::Union(it)) => (cx.vsema.source(it)?.map(|it| it.syntax()), "union"),
        hir::GenericDef::Trait(it) => (cx.vsema.source(it)?.map(|it| it.syntax()), "trait"),
        hir::GenericDef::TypeAlias(it) => {
            let kind = if it.as_assoc_item(db).is_some() { "associated type" } else { "type alias" };
            (cx.vsema.source(it)?.map(|it| it.syntax()), kind)
        }
        hir::GenericDef::Const(it) => {
            let kind = if it.as_assoc_item(db).is_some() { "associated constant" } else { "constant" };
            (cx.vsema.source(it)?.map(|it| it.syntax()), kind)
        }
        hir::GenericDef::Static(it) => (cx.vsema.source(it)?.map(|it| it.syntax()), "static"),
        hir::GenericDef::Impl(_) => return None,
    };
    let name = node.value.children().find_map(ast::Name::cast)?;
    Some((cx.span(node.with_value(name.syntax().text_range())), kind))
}

/// rustc's "consider relaxing the implicit `Sized` restriction" for a `Sized` bound required
/// by a local trait's associated type that has no default and writes no `Sized` bound.
fn relax_sized(cx: &Cx<'_, '_>, diag: &mut Diag, item: hir::GenericDef, trait_: Option<hir::Trait>) {
    let hir::GenericDef::TypeAlias(alias) = item else { return };
    if !trait_.is_some_and(|it| is_lang(cx, it, LangItem::Sized))
        || alias.as_assoc_item(cx.db).and_then(|it| it.container_trait(cx.db)).is_none()
        || alias.module(cx.db).krate(cx.db) != cx.krate
    {
        return;
    }
    let Some(src) = cx.vsema.source(alias) else { return };
    let ty = src.value;
    if ty.ty().is_some() {
        return;
    }
    let bounds = ty.type_bound_list();
    let sized = |bound| bound_trait(cx, bound).is_some_and(|it| is_lang(cx, it, LangItem::Sized));
    if bounds.into_iter().flat_map(|it| it.bounds()).any(sized) {
        return;
    }
    let (at, separator) = match bounds.and_then(|it| it.bounds().last()) {
        Some(last) => (last.syntax().text_range().end(), " +"),
        None => {
            let generics = ty.generic_param_list().map(|it| it.syntax());
            let Some(before) = generics.or(ty.name().map(|it| it.syntax())) else { return };
            (before.text_range().end(), ":")
        }
    };
    diag.suggestion_verbose(
        cx.span(src.with_value(TextRange::empty(at))),
        "consider relaxing the implicit `Sized` restriction",
        // alloc: the suggestion's text, for an error hir found
        format!("{separator} ?Sized"),
        Applicability::MachineApplicable,
    );
}

/// The trait a bound names.
fn bound_trait<'db>(cx: &Cx<'_, 'db>, bound: ast::TypeBound<'db>) -> Option<hir::Trait> {
    let ast::Type::PathType(ty) = bound.ty()? else { return None };
    match cx.vsema.resolve_path(ty.path()?)? {
        PathResolution::Def(hir::ModuleDef::Trait(it)) => Some(it),
        _ => None,
    }
}
