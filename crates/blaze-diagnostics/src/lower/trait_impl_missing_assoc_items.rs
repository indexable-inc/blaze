//! E0046: a trait impl without items the trait requires (hir's `TraitImplMissingAssocItems`),
//! with a label on each missing item of a trait of the crate and the suggested item rustc
//! writes for each (shown only to tools when the trait is local, as a help otherwise).
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/mod.rs` (`missing_items_err`,
//! `missing_items_suggestions`, `impl_suggestion_span`, `suggestion_signature`,
//! `fn_sig_suggestion`, `bounds_from_generic_clauses`), the structs `MissingTraitItem*` of
//! `compiler/rustc_hir_analysis/src/diagnostics.rs` and `ty_kind_suggestion` of
//! `compiler/rustc_trait_selection/src/error_reporting/traits/suggestions.rs`
//! (MIT or Apache-2.0).

use base_db::{CrateOrigin, LangCrateOrigin};
use blaze_items::{Fact, ItemId};
use hir::Mutability;
use hir_def::lang_item::lang_items;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _},
};

use crate::{
    cx::{Cx, item_range_view as item_range},
    lower::{
        Found, Phase,
        incoherent_impl::{ImplAt, assoc_item_span, unresolved},
    },
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::TraitImplMissingAssocItems) -> Vec<Found> {
    let Some(at) = ImplAt::of_ptr(cx, d.file_id, d.impl_) else {
        return unresolved(cx, d.file_id, d.impl_, "a missing trait items error");
    };
    // rustc checks an impl's items against its trait only when the trait is coherent
    // (`check_item_type`).
    if at.impl_.trait_(cx.db).is_some_and(|it| !crate::checks::impls::coherent(cx, it)) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: the error's parts, for an error hir found
    let names: Vec<&str> = d.missing.iter().map(|(name, _)| name.as_str()).collect();
    let message = names.join("`, `");
    let span = at.span(cx);
    let mut diag =
        // alloc: the error's text, for an error hir found
        Diag::error(Some("E0046"), format!("not all trait items implemented, missing: `{message}`"), span.clone());
    // alloc: the error's text, for an error hir found
    diag.span_label(span, format!("missing `{message}` in implementation"));
    let sugg_span = suggestion_span(cx, &at);
    let padding = indentation_before(cx, &sugg_span);
    // alloc: the error's parts, for an error hir found
    let mut local = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut foreign = Vec::new();
    for (name, item) in &d.missing {
        let is_local = item.module(cx.db).krate(cx.db) == cx.krate;
        let label_at = if is_local { assoc_item_span(cx, *item) } else { None };
        // clone: on the error path, for an error hir found
        if let Some(label_at) = label_at.clone() {
            // alloc: the error's text, for an error hir found
            diag.span_label(label_at, format!("`{}` from trait", name.as_str()));
        }
        // An item whose signature hir cannot give as rustc writes it gets no suggestion
        // (v2/requests/diagnostics.md, `diagnostics-items-impls`).
        if let Some(snippet) = signature(cx, &at, *item) {
            // alloc: the error's text, for an error hir found
            let code = format!("{padding}{snippet}\n{padding}");
            match label_at.is_some() {
                true => local.push((snippet, code)),
                false => foreign.push((snippet, code)),
            }
        }
    }
    for (style, items) in [(SuggestionStyle::CompletelyHidden, local), (SuggestionStyle::HideCodeAlways, foreign)] {
        for (snippet, code) in items {
            diag.suggestion_with_style(
                // alloc: the error's parts, for an error hir found
                vec![(sugg_span.clone(), code)],
                // alloc: the error's text, for an error hir found
                format!("implement the missing item: `{snippet}`"),
                Applicability::HasPlaceholders,
                style,
            );
        }
    }
    diag.fact(at.coherence(cx), "the items of the impl");
    if let Some(trait_) = at.impl_.trait_(cx.db) {
        diag.fact(
            Fact::Signature(cx.key(ItemId::Trait(trait_.into()))),
            // alloc: the error's text, for an error hir found
            format!("the items `{}` requires", cx.def_path(hir::ModuleDef::Trait(trait_))),
        );
    }
    // alloc: the error's parts, for an error hir found
    vec![Found::new(diag, Phase::Items)]
}

/// `impl_suggestion_span`: just before the impl's closing brace, else after the impl.
fn suggestion_span(cx: &Cx<'_, '_>, at: &ImplAt<'_>) -> Span {
    let full = cx.span(at.node.map(|it| item_range(it.syntax())));
    match cx.snippet(&full) {
        Some(text) if text.ends_with('}') => {
            let hi = full.hi - 1;
            full.with_lo(hi).with_hi(hi)
        }
        _ => full.shrink_to_hi(),
    }
}

/// `SourceMap::indentation_before`: the white space that starts the line `span` begins on.
fn indentation_before(cx: &Cx<'_, '_>, span: &Span) -> String {
    // alloc: the error's text, for an error hir found
    let Some(file) = cx.file_of(span) else { return String::new() };
    let text = cx.file_text(file);
    let before = &text[..(span.lo as usize).min(text.len())];
    let line = before.rsplit_once('\n').map_or(before, |(_, line)| line);
    // alloc: the error's text, for an error hir found
    line.split_once(|c: char| !c.is_whitespace()).map_or(line, |(indent, _)| indent).to_owned()
}

/// `suggestion_signature`: the item rustc suggests adding, the trait's parameters replaced by
/// the impl's arguments and its `Self` kept.
fn signature(cx: &Cx<'_, '_>, at: &ImplAt<'_>, item: hir::AssocItem) -> Option<String> {
    match item {
        hir::AssocItem::Function(it) => fn_signature(cx, at, it),
        hir::AssocItem::TypeAlias(it) => {
            let source = cx.vsema.source(it)?;
            if !plain_generics(source.value.syntax()) {
                return None;
            }
            let (generics, where_clauses) = generics(cx, hir::GenericDef::TypeAlias(it));
            // alloc: the error's text, for an error hir found
            Some(format!("type {}{generics} = /* Type */{where_clauses};", it.name(cx.db).as_str()))
        }
        hir::AssocItem::Const(it) => {
            let ty = it.ty(cx.db);
            // alloc: the error's text, for an error hir found
            let value = value_of(cx, &ty).unwrap_or_else(|| "value".to_owned());
            let name = it.name(cx.db)?;
            // alloc: the error's text, for an error hir found
            Some(format!("const {}: {} = {value};", name.as_str(), cx.ty(&ty)))
        }
    }
}

/// `fn_sig_suggestion`.
fn fn_signature(cx: &Cx<'_, '_>, at: &ImplAt<'_>, f: hir::Function) -> Option<String> {
    let db = cx.db;
    let source = cx.vsema.source(f)?;
    if !plain_generics(source.value.syntax()) {
        return None;
    }
    let args = instantiation(cx, at, f)?;
    let is_method = f.has_self_param(db);
    let mut params: Vec<String> = f
        .assoc_fn_params(db)
        .iter()
        .enumerate()
        .map(|(i, param)| {
            let ty = param.ty().instantiate_type_params(db, args.iter());
            if is_method && i == 0 && ty.as_type_param(db).is_some() {
                // alloc: the error's text, for an error hir found
                return "self".to_owned();
            }
            if i == 0
                && let Some((inner, mutability)) = ty.as_reference()
            {
                let mutability = match mutability {
                    Mutability::Mut => "mut ",
                    Mutability::Shared => "",
                };
                return match is_method {
                    true if inner.as_type_param(db).is_some_and(|it| it.name(db).as_str() == "Self") => {
                        // alloc: the error's text, for an error hir found
                        format!("&{mutability}self")
                    }
                    // alloc: the error's text, for an error hir found
                    true => format!("self: {}", cx.ty(&ty)),
                    // alloc: the error's text, for an error hir found
                    false => format!("_: {}", cx.ty(&ty)),
                };
            }
            match is_method && i == 0 {
                // alloc: the error's text, for an error hir found
                true => format!("self: {}", cx.ty(&ty)),
                // alloc: the error's text, for an error hir found
                false => format!("_: {}", cx.ty(&ty)),
            }
        })
        // alloc: the error's parts, for an error hir found
        .collect();
    if f.is_varargs(db) {
        // alloc: the error's text, for an error hir found
        params.push("...".to_owned());
    }
    let (asyncness, output) = match f.is_async(db) {
        true => ("async ", f.async_ret_type(db)?),
        false => ("", f.ret_type(db)),
    };
    let output = output.instantiate_type_params(db, args.iter());
    let output = match output.is_unit() {
        // alloc: the error's text, for an error hir found
        true => String::new(),
        // alloc: the error's text, for an error hir found
        false => format!(" -> {}", cx.ty(&output)),
    };
    let safety = if f.is_unsafe(db) { "unsafe " } else { "" };
    let (generics, where_clauses) = generics(cx, hir::GenericDef::Function(f));
    let name = f.name(db);
    // alloc: the error's text, for an error hir found
    Some(format!(
        "{safety}{asyncness}fn {}{generics}({}){output}{where_clauses} {{ todo!() }}",
        name.as_str(),
        params.join(", ")
    ))
}

/// The arguments of the trait function's type parameters, in order: the trait's `Self` kept,
/// the trait's own parameters the impl's arguments, the function's own kept.
fn instantiation<'db>(cx: &Cx<'_, 'db>, at: &ImplAt<'_>, f: hir::Function) -> Option<Vec<hir::Type<'db>>> {
    let db = cx.db;
    let trait_ref = at.impl_.trait_ref(db)?;
    let trait_ = trait_ref.trait_();
    let trait_params = hir::GenericDef::Trait(trait_).params(db);
    // alloc: the error's parts, for an error hir found
    let mut args = Vec::new();
    let mut index = 0;
    for param in &trait_params {
        match param {
            hir::GenericParam::LifetimeParam(_) => index += 1,
            hir::GenericParam::ConstParam(_) => index += 1,
            hir::GenericParam::TypeParam(it) if it.is_implicit(db) => {
                args.push(it.ty(db));
                index += 1;
            }
            hir::GenericParam::TypeParam(_) => {
                args.push(trait_ref.get_type_argument(index)?);
                index += 1;
            }
        }
    }
    for param in hir::GenericDef::Function(f).params(db) {
        if let hir::GenericParam::TypeParam(it) = param {
            args.push(it.ty(db));
        }
    }
    Some(args)
}

/// `bounds_from_generic_clauses`: the item's own parameters, with the bounds that are not
/// `Sized` as a where clause.
fn generics(cx: &Cx<'_, '_>, def: hir::GenericDef) -> (String, String) {
    let db = cx.db;
    let sized = lang_items(db, cx.krate.base()).Sized;
    let meta_sized = lang_items(db, cx.krate.base()).MetaSized;
    // alloc: the error's parts, for an error hir found
    let mut params = Vec::new();
    // alloc: the error's parts, for an error hir found
    let mut where_clauses = Vec::new();
    for param in def.params(db) {
        match param {
            // alloc: the error's text, for an error hir found
            hir::GenericParam::LifetimeParam(it) => params.push(it.name(db).as_str().to_owned()),
            hir::GenericParam::TypeParam(it) => {
                if it.is_implicit(db) {
                    continue;
                }
                // alloc: the error's text, for an error hir found
                let name = it.name(db).as_str().to_owned();
                for bound in it.trait_bounds(db) {
                    let id = hir_def::TraitId::from(bound);
                    if Some(id) == sized {
                        continue;
                    }
                    let path = match Some(id) == meta_sized {
                        // alloc: the error's text, for an error hir found
                        true => "?Sized".to_owned(),
                        false => cx.def_path(hir::ModuleDef::Trait(bound)),
                    };
                    // alloc: the error's text, for an error hir found
                    where_clauses.push(format!("{name}: {path}"));
                }
                params.push(name);
            }
            hir::GenericParam::ConstParam(it) => {
                // alloc: the error's text, for an error hir found
                params.push(format!("const {}: {}", it.name(db).as_str(), cx.ty(&it.ty(db))));
            }
        }
    }
    let generics = match params.is_empty() {
        // alloc: the error's text, for an error hir found
        true => String::new(),
        // alloc: the error's text, for an error hir found
        false => format!("<{}>", params.join(", ")),
    };
    let where_clauses = match where_clauses.is_empty() {
        // alloc: the error's text, for an error hir found
        true => String::new(),
        // alloc: the error's text, for an error hir found
        false => format!(" where {}", where_clauses.join(", ")),
    };
    (generics, where_clauses)
}

/// Whether the item's signature has only what hir's queries give as rustc prints it: no
/// lifetimes but `'static` (hir erases late-bound ones), no bound with an associated type
/// (`Iterator<Item = A>`), no `?Sized` and no where clause on another type than a parameter.
fn plain_generics(node: ast::Node<'_>) -> bool {
    let signature_end = ast::Fn::cast(node)
        .and_then(|it| it.body())
        .map_or(node.text_range().end(), |body| body.syntax().text_range().start());
    let signature = TextRange::new(node.text_range().start(), signature_end);
    let in_signature = |it: &ast::Node<'_>| signature.contains_range(it.text_range());
    let lifetimes = node
        .descendants()
        .filter(in_signature)
        .filter_map(ast::Lifetime::cast)
        .any(|it| it.syntax().text() != "'static");
    let projections = node.descendants().filter(in_signature).any(|it| ast::AssocTypeArg::can_cast(it.kind()));
    let unsized_ = node
        .descendants()
        .filter(in_signature)
        .filter_map(ast::TypeBound::cast)
        .any(|it| it.question_mark_token().is_some() || it.for_binder().is_some());
    let where_clause = match ast::AnyHasGenericParams::cast(node).and_then(|it| it.where_clause()) {
        Some(clause) => clause.predicates().any(|pred| {
            pred.for_binder().is_some()
                || !matches!(pred.ty(), Some(ast::Type::PathType(ty))
                    if ty.path().is_some_and(|path| path.qualifier().is_none() && path.segment().is_some_and(|it| it.generic_arg_list().is_none())))
        }),
        None => false,
    };
    !(lifetimes || projections || unsized_ || where_clause)
}

/// `ty_kind_suggestion`: a value of `ty` to write.
fn value_of(cx: &Cx<'_, '_>, ty: &hir::Type<'_>) -> Option<String> {
    let db = cx.db;
    let items = lang_items(db, cx.krate.base());
    if ty.is_never() || ty.is_unknown() {
        return None;
    }
    if ty.is_bool() {
        // alloc: the error's text, for an error hir found
        return Some("false".to_owned());
    }
    if ty.is_char() {
        // alloc: the error's text, for an error hir found
        return Some("'x'".to_owned());
    }
    if ty.is_int_or_uint() {
        // alloc: the error's text, for an error hir found
        return Some("42".to_owned());
    }
    if ty.is_float() {
        // alloc: the error's text, for an error hir found
        return Some("3.14159".to_owned());
    }
    if ty.is_slice() {
        // alloc: the error's text, for an error hir found
        return Some("[]".to_owned());
    }
    if let Some(adt) = ty.as_adt() {
        let id = hir_def::AdtId::from(adt);
        let krate = adt.module(db).krate(db);
        // alloc: the error's parts, for an error hir found
        let args: Vec<_> = ty.type_arguments().collect();
        let is_alloc = matches!(krate.origin(db), CrateOrigin::Lang(LangCrateOrigin::Alloc));
        if is_alloc && adt.name(db).as_str() == "Vec" {
            // alloc: the error's text, for an error hir found
            return Some("vec![]".to_owned());
        }
        if items.String.is_some_and(|it| hir_def::AdtId::StructId(it) == id) {
            // alloc: the error's text, for an error hir found
            return Some("String::new()".to_owned());
        }
        if items.OwnedBox.is_some_and(|it| hir_def::AdtId::StructId(it) == id) {
            // alloc: the error's text, for an error hir found
            return Some(format!("Box::new({})", value_of(cx, args.first()?)?));
        }
        if items.Option.is_some_and(|it| hir_def::AdtId::EnumId(it) == id) {
            // alloc: the error's text, for an error hir found
            return Some("None".to_owned());
        }
        let result = items.ResultOk.map(|it| hir::Adt::Enum(hir::EnumVariant::from(it).parent_enum(db)));
        if result == Some(adt) {
            // alloc: the error's text, for an error hir found
            return Some(format!("Ok({})", value_of(cx, args.first()?)?));
        }
        let default = items.Default.map(hir::Trait::from);
        if default.is_some_and(|it| ty.impls_trait(db, it, &[])) {
            // alloc: the error's text, for an error hir found
            return Some("Default::default()".to_owned());
        }
        // alloc: the error's text, for an error hir found
        return Some("/* value */".to_owned());
    }
    if let Some((inner, mutability)) = ty.as_reference() {
        if inner.is_str() && mutability == Mutability::Shared {
            // alloc: the error's text, for an error hir found
            return Some("\"\"".to_owned());
        }
        let prefix = match mutability {
            Mutability::Mut => "mut ",
            Mutability::Shared => "",
        };
        // alloc: the error's text, for an error hir found
        return Some(format!("&{prefix}{}", value_of(cx, &inner)?));
    }
    if let Some((elem, len)) = ty.as_array(db) {
        return Some(match len {
            // alloc: the error's text, for an error hir found
            0 => "[]".to_owned(),
            // alloc: the error's text, for an error hir found
            _ if len == 1 || elem.is_copy(db) => format!("[{}; {len}]", value_of(cx, &elem)?),
            // alloc: the error's text, for an error hir found
            _ => "/* value */".to_owned(),
        });
    }
    if ty.is_tuple() {
        let fields = ty.tuple_fields(db);
        let values = fields.iter().map(|it| value_of(cx, it)).collect::<Option<Vec<_>>>()?;
        let comma = if fields.len() == 1 { "," } else { "" };
        // alloc: the error's text, for an error hir found
        return Some(format!("({}{comma})", values.join(", ")));
    }
    // alloc: the error's text, for an error hir found
    Some("/* value */".to_owned())
}
