//! What rustc's resolver checks of the generic parameters of an item and of the `for<>`
//! binders in its signature: a name declared twice, the parameters of the impl or trait
//! around included (E0403); a lifetime parameter named `'static` (E0262); a default naming a
//! parameter declared after it, or itself (E0128).
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`with_generic_param_rib`,
//! `visit_generic_params`), `ident.rs` (`validate_res_from_ribs`) and its
//! `NameAlreadyUsedInParameterList`, `StaticLifetimeIsReserved` and
//! `ForwardDeclaredGenericParam` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::ImplId;
use syntax::{
    TextRange, WalkEvent,
    view::{self as ast, AstNode as _, HasGenericParams as _, HasName as _},
};

use super::item_id;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    // alloc: empty, allocates only when there is an error
    let mut found = Vec::new();
    for &module in scopes {
        for def in module.declarations_iter(cx.db) {
            match def {
                hir::ModuleDef::Trait(it) => {
                    let Some(source) = cx.vsema.source(it) else { continue };
                    let item = item_id(cx.db, def);
                    let outer = check_item(cx, item, source.map(|it| it.syntax()), None, &mut found);
                    for assoc in it.items_iter(cx.db) {
                        check_assoc(cx, assoc, outer, &mut found);
                    }
                }
                hir::ModuleDef::Function(it) => {
                    let Some(source) = cx.vsema.source(it) else { continue };
                    check_item(cx, item_id(cx.db, def), source.map(|it| it.syntax()), None, &mut found);
                }
                hir::ModuleDef::Adt(it) => {
                    let Some(source) = cx.vsema.source(it) else { continue };
                    check_item(cx, item_id(cx.db, def), source.map(|it| it.syntax()), None, &mut found);
                }
                hir::ModuleDef::TypeAlias(it) => {
                    let Some(source) = cx.vsema.source(it) else { continue };
                    check_item(cx, item_id(cx.db, def), source.map(|it| it.syntax()), None, &mut found);
                }
                _ => {}
            }
        }
        for imp in module.impl_defs_iter(cx.db) {
            let Some(source) = cx.vsema.source(imp) else { continue };
            let item = ImplId::try_from(imp).ok().map(ItemId::Impl);
            let outer = check_item(cx, item, source.map(|it| it.syntax()), None, &mut found);
            for assoc in imp.items_iter(cx.db) {
                check_assoc(cx, assoc, outer, &mut found);
            }
        }
    }
    found
}

fn check_assoc<'db>(cx: &Cx<'_, 'db>, assoc: hir::AssocItem, outer: Option<List<'db>>, found: &mut Vec<Found>) {
    let node = match assoc {
        hir::AssocItem::Function(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        hir::AssocItem::Const(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        hir::AssocItem::TypeAlias(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
    };
    let Some(node) = node else { return };
    check_item(cx, item_id(cx.db, assoc.into()), node, outer, found);
}

type List<'t> = InFile<ast::GenericParamList<'t>>;

/// The name a generic parameter declares.
enum ParamName<'t> {
    Lifetime(ast::Lifetime<'t>),
    Name(ast::Name<'t>),
}

impl<'t> ParamName<'t> {
    fn of(param: ast::GenericParam<'t>) -> Option<ParamName<'t>> {
        match param {
            ast::GenericParam::LifetimeParam(it) => it.lifetime().map(ParamName::Lifetime),
            ast::GenericParam::TypeParam(it) => it.name().map(ParamName::Name),
            ast::GenericParam::ConstParam(it) => it.name().map(ParamName::Name),
        }
    }

    fn text(&self) -> &'t str {
        match self {
            ParamName::Lifetime(it) => it.text(),
            ParamName::Name(it) => it.text(),
        }
    }

    fn range(&self) -> TextRange {
        match self {
            ParamName::Lifetime(it) => it.syntax().text_range(),
            ParamName::Name(it) => it.syntax().text_range(),
        }
    }

    fn is_lifetime(&self) -> bool {
        matches!(self, ParamName::Lifetime(_))
    }
}

/// Checks the parameters of `node` and of the binders in it, `outer` being the parameters of
/// the impl or trait around it, and returns its own, in scope of the items inside it.
fn check_item<'db>(
    cx: &Cx<'_, 'db>,
    item: Option<ItemId>,
    node: InFile<ast::Node<'db>>,
    outer: Option<List<'db>>,
    found: &mut Vec<Found>,
) -> Option<List<'db>> {
    let fact = item.map(|it| Fact::Signature(cx.key(it)));
    let list = ast::AnyHasGenericParams::cast(node.value).and_then(|it| it.generic_param_list());
    if let Some(list) = list {
        check_list(cx, node.with_value(list), &[outer], fact.as_ref(), found);
        forward(cx, node.with_value(list), fact.as_ref(), found);
    }
    // The names in scope where a binder's list starts, outermost first.
    let inner = [outer, list.map(|it| node.with_value(it))];
    let mut walk = node.value.preorder();
    walk.next();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(child) = event else { continue };
        if ast::Item::can_cast(child.kind())
            || ast::AssocItemList::can_cast(child.kind())
            || ast::BlockExpr::can_cast(child.kind())
        {
            walk.skip_subtree();
            continue;
        }
        let Some(binder) = ast::ForBinder::cast(child) else { continue };
        let Some(list) = binder.generic_param_list() else { continue };
        check_list(cx, node.with_value(list), &inner, fact.as_ref(), found);
    }
    list.map(|it| node.with_value(it))
}

/// rustc's `with_generic_param_rib` over one parameter list, `outer` being the lists whose
/// names are in scope where it starts (rustc's `seen_bindings` of the type and value ribs
/// and its `seen_lifetimes`), outermost first: E0403 and E0262.
fn check_list(cx: &Cx<'_, '_>, list: List<'_>, outer: &[Option<List<'_>>], fact: Option<&Fact>, found: &mut Vec<Found>) {
    let outer = || outer.iter().flatten();
    // A lifetime shadowing one of the item around is E0496, which is not this check's.
    let shadows = |name: &ParamName<'_>| {
        name.is_lifetime()
            && outer().any(|it| {
                it.value.lifetime_params().filter_map(|it| it.lifetime()).any(|it| it.text() == name.text())
            })
    };
    for (at, param) in list.value.generic_params().enumerate() {
        let Some(name) = ParamName::of(param) else { continue };
        if shadows(&name) {
            continue;
        }
        let text = name.text();
        // The outermost declaration of a name wins, as rustc extends its map rib by rib
        // outwards; the lifetimes around are not in it.
        let first = outer()
            .find_map(|outer| {
                let mut names = outer.value.generic_params().filter_map(ParamName::of);
                names.find(|it| !it.is_lifetime() && it.text() == text).map(|it| outer.with_value(it.range()))
            })
            .or_else(|| {
                let mut names = list.value.generic_params().take(at).filter_map(ParamName::of);
                names.find(|it| !shadows(it) && it.text() == text).map(|it| list.with_value(it.range()))
            });
        if let Some(first) = first {
            let span = cx.span(list.with_value(name.range()));
            let mut diag = Diag::error(
                Some("E0403"),
                // alloc: the error's message
                format!(
                    "the name `{text}` is already used for a generic parameter in this item's generic parameters"
                ),
                // clone: the error's span
                span.clone(),
            );
            diag.span_label(span, "already used");
            // alloc: the error's label
            diag.span_label(cx.span(first), format!("first use of `{text}`"));
            push(diag, fact, found);
            continue;
        }
        if name.is_lifetime() && text == "'static" {
            let span = cx.span(list.with_value(name.range()));
            let mut diag = Diag::error(
                Some("E0262"),
                // alloc: the error's message
                format!("invalid lifetime parameter name: `{text}`"),
                // clone: the error's span
                span.clone(),
            );
            diag.span_label(span, "'static is a reserved lifetime name");
            push(diag, fact, found);
        }
    }
}

/// rustc's `ForwardGenericParamBan` ribs: a default may name only the parameters before it.
fn forward(cx: &Cx<'_, '_>, list: List<'_>, fact: Option<&Fact>, found: &mut Vec<Found>) {
    // Whether `text` names a type (or, with `is_const`, a const) parameter at or after the
    // `at`th and none before it.
    let banned_at = |at: usize, is_const: bool, text: &str| {
        let mut named = list.value.type_or_const_params().map(|param| match param {
            ast::TypeOrConstParam::Type(it) => !is_const && it.name().is_some_and(|it| it.text() == text),
            ast::TypeOrConstParam::Const(it) => is_const && it.name().is_some_and(|it| it.text() == text),
        });
        !named.by_ref().take(at).any(|it| it) && named.any(|it| it)
    };
    for (at, param) in list.value.type_or_const_params().enumerate() {
        let is_banned = |is_const: bool, text: &str| banned_at(at, is_const, text);
        match param {
            ast::TypeOrConstParam::Type(it) => {
                if let Some(default) = it.default_type() {
                    banned(cx, list.with_value(default.syntax()), is_banned, fact, found);
                }
            }
            ast::TypeOrConstParam::Const(it) => {
                if let Some(default) = it.default_val() {
                    banned(cx, list.with_value(default.syntax()), is_banned, fact, found);
                }
            }
        }
    }
}

/// Each path in `default` whose first segment names a banned parameter of its namespace. A
/// type argument that names no type is looked up as a value, as rustc does.
fn banned(
    cx: &Cx<'_, '_>,
    default: InFile<ast::Node<'_>>,
    is_banned: impl Fn(bool, &str) -> bool,
    fact: Option<&Fact>,
    found: &mut Vec<Found>,
) {
    for path in default.value.descendants().filter_map(ast::Path::cast) {
        if path.qualifier().is_some() {
            continue;
        }
        let Some(segment) = path.segment() else { continue };
        if segment.coloncolon_token().is_some() || segment.type_anchor().is_some() {
            continue;
        }
        let Some(name) = segment.name_ref() else { continue };
        let Some(parent) = path.syntax().parent() else { continue };
        let text = name.text();
        let hit = if ast::PathExpr::can_cast(parent.kind()) {
            is_banned(true, text)
        } else if ast::Path::can_cast(parent.kind()) {
            is_banned(false, text)
        } else if ast::PathType::can_cast(parent.kind()) {
            is_banned(false, text)
                || (is_banned(true, text)
                    && parent.parent().is_some_and(|it| ast::TypeArg::can_cast(it.kind())))
        } else {
            false
        };
        if !hit {
            continue;
        }
        let span = cx.span(default.with_value(name.syntax().text_range()));
        let mut diag = Diag::error(
            Some("E0128"),
            "generic parameter defaults cannot reference parameters before they are declared",
            // clone: the error's span
            span.clone(),
        );
        // alloc: the error's label
        diag.span_label(span, format!("cannot reference `{text}` before it is declared"));
        push(diag, fact, found);
    }
}

fn push(mut diag: Diag, fact: Option<&Fact>, found: &mut Vec<Found>) {
    if let Some(fact) = fact {
        // clone: the error's fact
        diag.fact(fact.clone(), "the generic parameters of the item");
    }
    found.push(Found::new(diag, Phase::Resolve));
}
