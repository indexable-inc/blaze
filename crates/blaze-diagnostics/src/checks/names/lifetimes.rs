//! A named lifetime of a signature that no binder of its item declares: E0261 "use of
//! undeclared lifetime name", or E0401 when an item around declares it; with `'static` for a
//! misspelling of it, else the lifetime introduced at each binder of the item, innermost first.
//! A lifetime in the type of a const parameter is E0770, not this.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`resolve_lifetime`, the lifetime ribs
//! of `visit_ty`, `visit_poly_trait_ref`, `visit_where_predicate` and `resolve_item`,
//! `LifetimeBinderKind::descr`) and `late/diagnostics.rs` (`emit_undeclared_lifetime_error`,
//! `suggest_introducing_lifetime_filtered`) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use stdx::edit_distance::edit_distance;
use syntax::{
    TextRange,
    view::{self as ast, AstNode, HasGenericParams as _, HasName as _, HasTypeBounds as _},
};

use super::{Signature, outer};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// rustc's `LifetimeBinderKind`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    FnPtrType,
    PolyTrait,
    WhereBound,
    Item,
    ConstItem,
    ImplAssocType,
    ImplBlock,
    Function,
}

impl Kind {
    fn descr(self) -> &'static str {
        match self {
            Kind::FnPtrType => "type",
            Kind::PolyTrait | Kind::WhereBound => "bound",
            Kind::Item | Kind::ConstItem => "item",
            Kind::ImplAssocType => "associated type",
            Kind::ImplBlock => "impl block",
            Kind::Function => "function",
        }
    }

    fn is_higher_ranked(self) -> bool {
        matches!(self, Kind::FnPtrType | Kind::PolyTrait | Kind::WhereBound)
    }
}

/// A lifetime rib of rustc's: what it declares, and its span (`for<..>` or `<..>`, or empty
/// where they would go).
struct Binder<'t> {
    kind: Kind,
    list: Option<ast::GenericParamList<'t>>,
    span: TextRange,
    node: ast::Node<'t>,
}

impl<'t> Binder<'t> {
    fn of(node: ast::Node<'t>) -> Option<Binder<'t>> {
        let binder = |kind, for_binder: Option<ast::ForBinder<'t>>, at: TextRange| {
            let list = for_binder.and_then(|it| it.generic_param_list());
            let span = for_binder.map_or(TextRange::empty(at.start()), |it| it.syntax().text_range());
            Some(Binder { kind, list, span, node })
        };
        if let Some(it) = cast::<ast::ForType>(node) {
            return binder(Kind::FnPtrType, it.for_binder(), it.syntax().text_range());
        }
        if let Some(it) = cast::<ast::FnPtrType>(node) {
            if node.parent().is_some_and(|it| ast::ForType::can_cast(it.kind())) {
                return None;
            }
            return binder(Kind::FnPtrType, None, it.syntax().text_range());
        }
        if let Some(it) = cast::<ast::TypeBound>(node) {
            let path = match it.ty()? {
                ast::Type::PathType(ty) => ty.syntax().text_range(),
                _ => return None,
            };
            return binder(Kind::PolyTrait, it.for_binder(), path);
        }
        if let Some(it) = cast::<ast::WherePred>(node) {
            it.ty()?;
            return binder(Kind::WhereBound, it.for_binder(), it.syntax().text_range());
        }
        let kind = match cast::<ast::Item>(node)? {
            ast::Item::Fn(_) => Kind::Function,
            ast::Item::Struct(_) | ast::Item::Enum(_) | ast::Item::Union(_) | ast::Item::Trait(_) => {
                Kind::Item
            }
            ast::Item::TypeAlias(_) => {
                let in_impl = node
                    .parent()
                    .filter(|it| ast::AssocItemList::can_cast(it.kind()))
                    .and_then(|it| it.parent())
                    .is_some_and(|it| ast::Impl::can_cast(it.kind()));
                if in_impl { Kind::ImplAssocType } else { Kind::Item }
            }
            ast::Item::Const(_) => Kind::ConstItem,
            ast::Item::Impl(_) => Kind::ImplBlock,
            _ => return None,
        };
        let list = cast::<ast::AnyHasGenericParams>(node)?.generic_param_list();
        let span = match &list {
            Some(it) => it.syntax().text_range(),
            None => {
                let after = match cast::<ast::AnyHasName>(node).and_then(|it| it.name()) {
                    Some(name) => name.syntax().text_range().end(),
                    None => cast::<ast::Impl>(node)?.impl_token()?.text_range().end(),
                };
                TextRange::empty(after)
            }
        };
        Some(Binder { kind, list, span, node })
    }

    fn declares(&self, name: &str) -> Option<ast::Lifetime<'t>> {
        declared(self.list?, name)
    }
}

/// `node` as an `N`, if it is one.
fn cast<'t, N: AstNode<'t>>(node: ast::Node<'t>) -> Option<N> {
    N::cast(node)
}

fn declared<'t>(list: ast::GenericParamList<'t>, name: &str) -> Option<ast::Lifetime<'t>> {
    list.lifetime_params().filter_map(|it| it.lifetime()).find(|it| it.text() == name)
}

/// The impl or trait among whose items a macro call emitted `item`, through the calls of
/// expansions emitted at the top of another: its generics are a rib of `item`'s, as in rustc,
/// where a `macro_rules!` lifetime has no hygiene and resolves where the macro expands.
fn around_call<'db>(cx: &Cx<'_, 'db>, mut file: HirFileId, item: ast::Node<'db>) -> Option<ast::Node<'db>> {
    if item.parent()?.parent().is_some() {
        return None;
    }
    loop {
        let call = file.macro_file()?.loc(cx.db).to_node_view(cx.db);
        let parent = call.value.parent()?;
        if ast::AssocItemList::can_cast(parent.kind()) {
            return parent.parent();
        }
        if parent.parent().is_some() {
            return None;
        }
        file = call.file_id;
    }
}

pub(super) fn check<'db>(cx: &Cx<'_, 'db>, signature: &Signature<'db>, lifetime: ast::Lifetime<'db>) -> Option<Found> {
    let name = lifetime.text();
    if name == "'static" || name == "'_" {
        return None;
    }
    if lifetime.syntax().parent().is_some_and(|it| ast::LifetimeParam::can_cast(it.kind())) {
        return None;
    }
    let inner = outer::inner_item(signature.node.value);
    // Almost every lifetime is declared by a binder around it: that is found first, with
    // nothing collected; the binders are gathered for the error alone.
    let stop = inner;
    let ribs = || {
        let mut ancestors = lifetime.syntax().ancestors();
        let mut done = false;
        std::iter::from_fn(move || {
            if done {
                return None;
            }
            let node = ancestors.next()?;
            done = node == stop;
            Some(node)
        })
    };
    let around = match inner == signature.node.value {
        true => around_call(cx, signature.node.file_id, inner),
        false => None,
    };
    for node in ribs().chain(around) {
        if ast::ConstParam::can_cast(node.kind()) {
            return None;
        }
        if Binder::of(node).is_some_and(|it| it.declares(name).is_some()) {
            return None;
        }
    }
    // alloc: on the error path, every binder having been passed
    let binders: Vec<Binder<'_>> = ribs().filter_map(Binder::of).collect();
    let outer = inner
        .ancestors()
        .skip(1)
        .take_while(|it| !ast::Module::can_cast(it.kind()) && !ast::SourceFile::can_cast(it.kind()))
        .filter_map(Binder::of)
        .find_map(|it| it.declares(name));

    let file = signature.node.file_id;
    let at = |range: TextRange| cx.span(InFile::new(file, range));
    let span = at(lifetime.syntax().text_range());
    let mut diag = match &outer {
        Some(outer) => {
            // clone: the error's span
            let mut diag = Diag::error(Some("E0401"), "can't use generic parameters from outer item", span.clone());
            // clone: the error's span
            diag.span_label(span.clone(), "use of generic parameter from outer item");
            diag.span_label(at(outer.syntax().text_range()), "lifetime parameter from outer item");
            diag
        }
        None => {
            // alloc: the error's message and span
            let mut diag = Diag::error(Some("E0261"), format!("use of undeclared lifetime name `{name}`"), span.clone());
            // clone: the error's span
            diag.span_label(span.clone(), "undeclared lifetime");
            diag
        }
    };
    if edit_distance(name, "'static", 2).is_some() {
        diag.suggestion_verbose(span, "you may have misspelled the `'static` lifetime", "'static", Applicability::MachineApplicable);
    } else {
        let generic_const_items = hir_def::unstable_features::UnstableFeatures::query(cx.db, cx.krate.base())
            .iter()
            .any(|it| it.as_str() == "generic_const_items");
        let mut noted = false;
        for binder in &binders {
            if binder.kind == Kind::ImplAssocType || (binder.kind == Kind::ConstItem && !generic_const_items) {
                continue;
            }
            let higher_ranked = binder.kind.is_higher_ranked();
            // alloc: the error's suggestion
            let mut removed: Vec<TextRange> = Vec::new();
            let (insert, sugg) = if binder.span.is_empty() {
                // alloc: the error's suggestion
                let mut names = vec![name.to_owned()];
                if binder.kind == Kind::WhereBound
                    && let Some(pred) = ast::WherePred::cast(binder.node)
                    && let Some(bounds) = pred.type_bound_list()
                {
                    for bound in bounds.bounds() {
                        let (Some(for_binder), Some(ast::Type::PathType(ty))) = (bound.for_binder(), bound.ty()) else {
                            continue;
                        };
                        let start = bound.syntax().text_range().start();
                        removed.push(TextRange::new(start, ty.syntax().text_range().start()));
                        for param in for_binder.generic_param_list().iter().flat_map(|it| it.lifetime_params()) {
                            if let Some(it) = param.lifetime()
                                && !names.iter().any(|name| *name == it.text())
                            {
                                // alloc: the error's suggestion
                                names.push(it.text().to_string());
                            }
                        }
                    }
                }
                let names = names.join(", ");
                // alloc: the error's suggestion
                let sugg = if higher_ranked { format!("for<{names}> ") } else { format!("<{names}>") };
                (binder.span, sugg)
            } else {
                let Some(angle) = binder.list.as_ref().and_then(|it| it.l_angle_token()) else { continue };
                // alloc: the error's suggestion
                (TextRange::empty(angle.text_range().end()), format!("{name}, "))
            };
            let message = if higher_ranked {
                // alloc: the error's suggestion
                format!("consider making the {} lifetime-generic with a new `{name}` lifetime", binder.kind.descr())
            } else {
                // alloc: the error's suggestion
                format!("consider introducing lifetime `{name}` here")
            };
            // alloc: the error's suggestion
            let parts = std::iter::once((at(insert), sugg)).chain(removed.into_iter().map(|it| (at(it), String::new()))).collect();
            diag.multipart_suggestion(message, parts, Applicability::MaybeIncorrect);
            if higher_ranked && !noted {
                noted = true;
                diag.note("for more information on higher-ranked polymorphism, visit https://doc.rust-lang.org/nomicon/hrtb.html");
            }
        }
    }
    signature.fact(cx, &mut diag);
    Some(Found::new(diag, Phase::Resolve))
}
