//! Lifetimes left out of `&T`, written `'_` or left out of a path where there is nothing to
//! elide to: E0106 "missing lifetime specifier" for each in an item (a field, a free type
//! alias), one E0106 for every one of a return type whose parameters give no lifetime (with
//! what the parameters have), and where an elided lifetime is not allowed (a bound, a where
//! clause, a const parameter's type, a default, an associated type) E0637 "`&` without an
//! explicit lifetime name cannot be used here" or "`'_` cannot be used here", or for `&` in a
//! trait impl's associated type "missing lifetime in associated type".
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_anonymous_lifetime`,
//! `point_at_impl_lifetimes`, `resolve_fn_signature`), `compiler/rustc_resolve/src/late/
//! diagnostics.rs` (`report_missing_lifetime_specifiers`,
//! `suggest_introducing_lifetime_for_assoc_ty_binding`) and `compiler/rustc_resolve/src/
//! diagnostics/mod.rs` (`ElidedAnonymousLifetimeReportError`,
//! `ExplicitAnonymousLifetimeReportError`, `AnonymousLifetimeNonGatReportError`,
//! `LendingIteratorReportError`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::{GenericDef, HirFileId, InFile, LangItem, MissingLifetimeKind, MissingLifetimeSite};
use syntax::{
    TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use crate::{
    cx::{Cx, item_range_view},
    lower::{
        Found, Phase,
        ambiguous_associated_item::assoc_type_span,
        incorrect_generics_len::{Segment, generics_fact, site_fact},
        missing_lifetime::{At, Binder, Kind, Missing, Sig, fn_params, report, return_of, ribs},
    },
    model::{Applicability, Diag, MultiSpan, Span},
};

/// A site as its syntax has it: where it is, the node it is, and the definition a path names.
struct Site<'db> {
    file: HirFileId,
    node: ast::Node<'db>,
    missing: Missing,
    def: Option<GenericDef>,
}

fn site<'db>(cx: &Cx<'_, 'db>, site: &MissingLifetimeSite) -> Option<Site<'db>> {
    Some(match site {
        MissingLifetimeSite::Reference(ptr) => {
            let it = cx.node_view(*ptr);
            let amp = it.value.amp_token()?;
            Site { file: it.file_id, node: it.value.syntax(), missing: Missing::ampersand(amp.text_range()), def: None }
        }
        MissingLifetimeSite::Placeholder(ptr) => {
            let it = cx.node_view(*ptr);
            let range = it.value.syntax().text_range();
            Site { file: it.file_id, node: it.value.syntax(), missing: Missing::underscore(range), def: None }
        }
        MissingLifetimeSite::Path { generics_or_segment, expected, def } => {
            let seg = Segment::of(cx, *generics_or_segment)?;
            let missing = Missing::of_segment(&seg, *expected as usize);
            Site { file: seg.file, node: seg.node, missing, def: Some(*def) }
        }
    })
}

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MissingLifetimes) -> Vec<Found> {
    // alloc: the error's parts, for an error hir found
    let mut sites: Vec<Site<'_>> = d.sites.iter().filter_map(|it| site(cx, it)).collect();
    sites.sort_unstable_by_key(|it| it.missing.range.start());
    // alloc: empty, allocates nothing
    let Some(first) = sites.first() else { return Vec::new() };
    match d.kind {
        // One error for every site, as rustc's `resolve_fn_signature` reports them.
        MissingLifetimeKind::ElisionFailure => {
            let (file, node) = (first.file, first.node);
            // alloc: the error's parts, for an error hir found
            let refs: Vec<Missing> = sites.iter().map(|it| it.missing).collect();
            let sig = return_of(node);
            let params = sig.map(|sig| fn_params(cx, sig));
            let rib_at = sig.map_or(node, Sig::syntax);
            let mut diag = report(cx, file, rib_at, &refs, params);
            for it in &sites {
                if let Some(def) = it.def {
                    generics_fact(cx, &mut diag, def);
                }
            }
            site_fact(cx, &mut diag, InFile::new(file, node));
            // alloc: the error's parts, for an error hir found
            vec![Found::new(diag, Phase::Resolve)]
        }
        MissingLifetimeKind::Missing | MissingLifetimeKind::NotAllowed => sites
            .iter()
            .map(|it| {
                let mut diag = match (d.kind, it.missing.kind) {
                    (MissingLifetimeKind::NotAllowed, Kind::Underscore) => underscore(cx, it),
                    (MissingLifetimeKind::NotAllowed, Kind::Ampersand) => ampersand(cx, it),
                    _ => report(cx, it.file, it.node, std::slice::from_ref(&it.missing), None),
                };
                if let Some(def) = it.def {
                    generics_fact(cx, &mut diag, def);
                }
                site_fact(cx, &mut diag, InFile::new(it.file, it.node));
                Found::new(diag, Phase::Resolve)
            })
            // alloc: the error's parts, for an error hir found
            .collect(),
    }
}

/// `ExplicitAnonymousLifetimeReportError`.
fn underscore(cx: &Cx<'_, '_>, site: &Site<'_>) -> Diag {
    let span = cx.span(InFile::new(site.file, site.missing.range));
    // clone: the error's span, for an error hir found
    let mut diag = Diag::error(Some("E0637"), "`'_` cannot be used here", span.clone());
    diag.span_label(span, "`'_` is a reserved lifetime name");
    diag
}

/// `&` where an elided lifetime is not allowed: in a trait impl's associated type without
/// generics, "missing lifetime in associated type" (or of `Iterator::Item`); else E0637 with
/// a lifetime to introduce.
fn ampersand<'db>(cx: &Cx<'_, 'db>, site: &Site<'db>) -> Diag {
    let span = |range: TextRange| cx.span(InFile::new(site.file, range));
    let amp = span(site.missing.range);
    if let Some((impl_, alias)) = non_gat_assoc_type(site.node)
        && !in_fn_body(site.node)
    {
        return assoc_type(cx, site, impl_, alias, amp);
    }
    let mut diag = Diag::error(
        Some("E0637"),
        "`&` without an explicit lifetime name cannot be used here",
        // clone: the error's span, for an error hir found
        amp.clone(),
    );
    // clone: the error's span, for an error hir found
    diag.span_label(amp.clone(), "explicit lifetime name needed here");
    if in_assoc_ty_binding(site.node) {
        introduce_for_binding(cx, &mut diag, site, &amp);
    } else if let Some(lo) = higher_ranked_binder(site.node) {
        diag.multipart_suggestion(
            "consider introducing a higher-ranked lifetime here",
            // alloc: the error's parts, for an error hir found
            vec![
                // alloc: the error's text, for an error hir found
                (span(TextRange::empty(lo)), "for<'a> ".to_owned()),
                // alloc: the error's text, for an error hir found
                (amp.shrink_to_hi(), "'a ".to_owned()),
            ],
            Applicability::MachineApplicable,
        );
    }
    diag
}

/// The impl of a trait and its associated type without generic parameters that `node` is in
/// (rustc's `current_trait_ref` with `in_non_gat_assoc_type` of `Some(true)`).
fn non_gat_assoc_type(node: ast::Node<'_>) -> Option<(ast::Impl<'_>, ast::TypeAlias<'_>)> {
    let item = node.ancestors().find_map(ast::Item::cast)?;
    let ast::Item::TypeAlias(alias) = item else { return None };
    if alias.generic_param_list().is_some() {
        return None;
    }
    let list = alias.syntax().parent().and_then(ast::AssocItemList::cast)?;
    let impl_ = list.syntax().parent().and_then(ast::Impl::cast)?;
    impl_.trait_()?;
    Some((impl_, alias))
}

/// rustc's `in_func_body`: `node` is in a function's body.
fn in_fn_body(node: ast::Node<'_>) -> bool {
    node.ancestors().any(|it| {
        ast::BlockExpr::can_cast(it.kind()) && it.parent().is_some_and(|it| ast::Fn::can_cast(it.kind()))
    })
}

/// rustc's `in_assoc_ty_binding`: `node` is in the type of `Name = Type`.
fn in_assoc_ty_binding(node: ast::Node<'_>) -> bool {
    let range = node.text_range();
    for ancestor in node.ancestors() {
        if ast::Item::can_cast(ancestor.kind()) {
            return false;
        }
        if let Some(arg) = ast::AssocTypeArg::cast(ancestor)
            && arg.eq_token().is_some()
            && arg.ty().is_some_and(|ty| ty.syntax().text_range().contains_range(range))
        {
            return true;
        }
    }
    false
}

/// Where rustc's `for<'a> ` goes: the start of the innermost bound (`PolyTrait`) or where
/// predicate (`WhereBound`) inside the rib that disallows the lifetime, none inside the
/// arguments of an associated type, whose own rib that is.
fn higher_ranked_binder(node: ast::Node<'_>) -> Option<syntax::TextSize> {
    for ancestor in node.ancestors() {
        let kind = ancestor.kind();
        if ast::Item::can_cast(kind) || ast::GenericParam::can_cast(kind) {
            return None;
        }
        if ast::GenericArgList::can_cast(kind)
            && ancestor.parent().is_some_and(|it| ast::AssocTypeArg::can_cast(it.kind()))
        {
            return None;
        }
        if let Some(bound) = ast::TypeBound::cast(ancestor)
            && bound.ty().is_some()
        {
            return Some(ancestor.text_range().start());
        }
        if let Some(pred) = ast::WherePred::cast(ancestor)
            && pred.ty().is_some()
        {
            return Some(ancestor.text_range().start());
        }
    }
    None
}

/// `suggest_introducing_lifetime_for_assoc_ty_binding`: a named lifetime on the nearest
/// generics that are not a binder.
fn introduce_for_binding(cx: &Cx<'_, '_>, diag: &mut Diag, site: &Site<'_>, amp: &Span) {
    let generic_const_items =
        cx.krate.is_unstable_feature_enabled(cx.db, &intern::Symbol::intern("generic_const_items"));
    for rib in ribs(site.node) {
        if matches!(rib.kind, Binder::FnPtrType | Binder::PolyTrait | Binder::WhereBound | Binder::ImplAssocType)
            || (rib.kind == Binder::ConstItem && !generic_const_items)
        {
            continue;
        }
        let (at, sugg) = match rib.at {
            // alloc: the error's text, for an error hir found
            At::Empty(at) => (at, "<'a>".to_owned()),
            // alloc: the error's text, for an error hir found
            At::AfterAngle(at) => (at, "'a, ".to_owned()),
        };
        diag.multipart_suggestion(
            "consider introducing a named lifetime parameter",
            // alloc: the error's parts, for an error hir found
            vec![
                (cx.span(InFile::new(site.file, TextRange::empty(at))), sugg),
                // alloc: the error's text, for an error hir found
                (amp.shrink_to_hi(), "'a ".to_owned()),
            ],
            Applicability::MaybeIncorrect,
        );
        break;
    }
}

const NON_GAT_NOTE: &str = "in the trait the associated type is declared without lifetime parameters, so \
                            using a borrowed type for them requires that lifetime to come from the \
                            implemented type";

/// `&` in a trait impl's associated type: `LendingIteratorReportError` for `Iterator`, else
/// `AnonymousLifetimeNonGatReportError` and `point_at_impl_lifetimes`.
fn assoc_type<'db>(
    cx: &Cx<'_, 'db>,
    site: &Site<'db>,
    impl_: ast::Impl<'db>,
    alias: ast::TypeAlias<'db>,
    amp: Span,
) -> Diag {
    let db = cx.db;
    let span = |range: TextRange| cx.span(InFile::new(site.file, range));
    let trait_ = cx.vsema.to_def(impl_).and_then(|it| it.trait_(db));
    let iterator = hir::Trait::lang(db, cx.krate, LangItem::Iterator);
    if trait_.is_some() && trait_ == iterator {
        let mut diag = Diag::error(
            None,
            "associated type `Iterator::Item` is declared without lifetime parameters, so using a \
             borrowed type for them requires that lifetime to come from the implemented type",
            amp,
        );
        if let Some(ty) = impl_.self_ty() {
            diag.span_note(
                span(ty.syntax().text_range()),
                "you can't create an `Iterator` that borrows each `Item` from itself, but you can \
                 instead create a new type that borrows your existing type and implement \
                 `Iterator` for that new type",
            );
        }
        return diag;
    }

    // clone: the error's span, for an error hir found
    let mut diag = Diag::error(None, "missing lifetime in associated type", amp.clone());
    // clone: the error's span, for an error hir found
    diag.span_label(amp.clone(), "this lifetime must come from the implemented type");
    let decl = trait_.filter(|it| it.module(db).krate(db) != cx.krate).and_then(|tr| {
        let name = alias.name()?;
        let item = tr
            .items_iter(db)
            .filter_map(|it| it.as_type_alias())
            .find(|it| it.name(db).as_str() == name.text())?;
        // alloc: the error's text, for an error hir found
        let what = format!("the associated types of `{}`", tr.name(db).display(db, cx.edition));
        diag.fact(Fact::Signature(cx.key(ItemId::Trait(tr.into()))), what);
        let primary = assoc_type_span(cx, item)?;
        // alloc: the error's parts, for an error hir found
        let labels = trait_span(cx, tr).map(|it| (it, String::new())).into_iter().collect();
        // alloc: the error's parts, for an error hir found
        Some(MultiSpan { primary: vec![primary], labels })
    });
    match decl {
        Some(decl) => diag.span_note(decl, NON_GAT_NOTE),
        None => diag.note(NON_GAT_NOTE),
    };
    point_at_impl_lifetimes(cx, &mut diag, site.file, impl_, &amp);
    diag
}

/// rustc's `def_span` of a trait: up to its last supertrait, else its name and generics.
fn trait_span(cx: &Cx<'_, '_>, tr: hir::Trait) -> Option<Span> {
    let source = cx.vsema.source(tr)?;
    let it = source.value;
    let start = item_range_view(it.syntax()).start();
    let end = match it.type_bound_list().and_then(|it| it.bounds().last()) {
        Some(bound) => bound.syntax().text_range().end(),
        None => match (it.generic_param_list(), it.name()) {
            (Some(list), _) => list.syntax().text_range().end(),
            (None, Some(name)) => name.syntax().text_range().end(),
            (None, None) => it.syntax().text_range().end(),
        },
    };
    Some(cx.span(source.with_value(TextRange::new(start, end.max(start)))))
}

/// `point_at_impl_lifetimes`: the impl's named lifetimes to use, or one to add to it.
fn point_at_impl_lifetimes(cx: &Cx<'_, '_>, diag: &mut Diag, file: HirFileId, impl_: ast::Impl<'_>, amp: &Span) {
    let span = |range: TextRange| cx.span(InFile::new(file, range));
    let list = impl_.generic_param_list();
    let generics = match list {
        Some(list) => list.syntax().text_range(),
        None => match impl_.impl_token() {
            Some(token) => TextRange::empty(token.text_range().end()),
            None => TextRange::empty(impl_.syntax().text_range().start()),
        },
    };
    // alloc: the error's parts, for an error hir found
    let named: Vec<_> = list.into_iter().flat_map(|it| it.lifetime_params()).filter_map(|it| it.lifetime()).collect();
    if !named.is_empty() {
        let (is, s) = if named.len() == 1 { ("is a", "") } else { ("are", "s") };
        // alloc: the error's text, for an error hir found
        diag.span_label(span(generics), format!("there {is} named lifetime{s} specified on the impl block you could use"));
        if let [lifetime] = &named[..] {
            diag.suggestion_verbose(
                amp.shrink_to_hi(),
                "consider using the lifetime from the impl block",
                // alloc: the error's text, for an error hir found
                format!("{} ", lifetime.text()),
                Applicability::MaybeIncorrect,
            );
        }
        return;
    }
    let in_self = impl_.self_ty().and_then(|it| anon_ref(it.syntax()));
    let in_trait = match in_self {
        Some(_) => None,
        None => impl_.trait_().and_then(|it| anon_ref(it.syntax())),
    };
    let (message, at, code) = match (in_self, in_trait) {
        (Some(at), _) => ("add a lifetime to the impl block and use it in the self type and associated type", at, "'a "),
        (None, Some(at)) => ("add a lifetime to the impl block and use it in the trait and associated type", at, "'a"),
        (None, None) => {
            diag.span_label(
                span(generics),
                "you could add a lifetime on the impl block, if the trait or the self type could have one",
            );
            return;
        }
    };
    diag.multipart_suggestion(
        message,
        // alloc: the error's parts, for an error hir found
        vec![
            // alloc: the error's text, for an error hir found
            (span(generics), "<'a>".to_owned()),
            // alloc: the error's text, for an error hir found
            (span(at), code.to_owned()),
            // alloc: the error's text, for an error hir found
            (amp.shrink_to_hi(), "'a ".to_owned()),
        ],
        Applicability::MaybeIncorrect,
    );
}

/// rustc's `AnonRefFinder`: the first `&T` without a lifetime (where its type starts) or `'_`.
fn anon_ref(node: ast::Node<'_>) -> Option<TextRange> {
    let mut preorder = node.preorder();
    while let Some(event) = preorder.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if let Some(it) = ast::RefType::cast(node)
            && it.lifetime().is_none()
            && let Some(ty) = it.ty()
        {
            return Some(TextRange::empty(ty.syntax().text_range().start()));
        }
        if let Some(it) = ast::Lifetime::cast(node)
            && it.text() == "'_"
        {
            return Some(it.syntax().text_range());
        }
    }
    None
}
