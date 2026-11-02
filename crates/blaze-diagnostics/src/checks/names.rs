//! What rustc's resolver says of the names in the signatures of items, where hir resolves them
//! and reports nothing: the paths of types and traits in fields, signatures, impl headers,
//! bounds, where clauses, generic defaults and type aliases (E0425, E0405, E0404, E0573, E0433,
//! E0411, E0401, E0603), their lifetimes (E0261, E0401), and re-exports more visible than
//! their items (E0364, E0365). The names in bodies and in the constants of types (array
//! lengths) are the front end's: `UnresolvedIdent` of their bodies and anon consts.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`smart_resolve_path_fragment`,
//! `resolve_lifetime`, `PathSource`), `late/diagnostics.rs` and `imports.rs` (MIT or
//! Apache-2.0).

pub(crate) mod failure;
mod lifetimes;
mod outer;
pub(crate) mod paths;
mod privacy;
mod reexports;

use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::ImplId;
use syntax::{
    SyntaxKind, WalkEvent,
    view::{self as ast, AstNode as _},
};

use super::defs::item_id;
use crate::{cx::Cx, lower::Found, model::Diag};

/// The imports first, as rustc finalizes them before it resolves the items. `scopes` is
/// `defs::scopes`, made once per check.
pub(crate) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    let mut found = reexports::check(cx, scopes);
    signatures(cx, scopes, |signature| walk(cx, &signature, &mut found));
    found
}

/// An item whose signature is walked: its syntax, in a tree `cx.vsema` gave out, and the item
/// whose signature its errors read.
pub(super) struct Signature<'db> {
    pub(super) node: InFile<ast::Node<'db>>,
    pub(super) item: Option<ItemId>,
    /// Whether a primitive type's name is that type wherever no generic parameter of the
    /// signature has it: nothing of the crate shadows one, and the item is not in a block
    /// (`hir::Crate::shadows_primitive_type` counts no block scope).
    pub(super) primitives: bool,
}

impl Signature<'_> {
    /// The fact of the item's signature an error of it reads (its key is asked for here, at
    /// the edge, not for every item walked).
    pub(super) fn fact(&self, cx: &Cx<'_, '_>, diag: &mut Diag) {
        if let Some(item) = self.item {
            diag.fact(Fact::Signature(cx.key(item)), "the signature of the item");
        }
    }
}

/// Each item of the crate's scopes given to `visit`, in rustc's order, those of traits and
/// impls after them.
fn signatures<'db>(cx: &Cx<'_, 'db>, scopes: &[hir::Module], mut visit: impl FnMut(Signature<'db>)) {
    let db = cx.db;
    let unshadowed = !cx.krate.shadows_primitive_type(db);
    let mut push = |node: Option<InFile<ast::Node<'db>>>, item: Option<ItemId>, primitives: bool| {
        let Some(node) = node else { return };
        visit(Signature { node, item, primitives });
    };
    for &module in scopes {
        let primitives = unshadowed && !hir_def::ModuleId::from(module).is_block_module(db);
        for def in module.declarations_iter(db) {
            let item = item_id(db, def);
            match def {
                hir::ModuleDef::Function(it) => push(syntax_of(cx.vsema.source(it)), item, primitives),
                hir::ModuleDef::Adt(it) => push(syntax_of(cx.vsema.source(it)), item, primitives),
                hir::ModuleDef::TypeAlias(it) => push(syntax_of(cx.vsema.source(it)), item, primitives),
                hir::ModuleDef::Const(it) => push(syntax_of(cx.vsema.source(it)), item, primitives),
                hir::ModuleDef::Static(it) => push(syntax_of(cx.vsema.source(it)), item, primitives),
                hir::ModuleDef::Trait(it) => {
                    push(syntax_of(cx.vsema.source(it)), item, primitives);
                    for assoc in it.items_iter(db) {
                        push(assoc_syntax(cx, assoc), item_id(db, assoc.into()), primitives);
                    }
                }
                _ => {}
            }
        }
        for imp in module.impl_defs_iter(db) {
            push(syntax_of(cx.vsema.source(imp)), ImplId::try_from(imp).ok().map(ItemId::Impl), primitives);
            for assoc in imp.items_iter(db) {
                push(assoc_syntax(cx, assoc), item_id(db, assoc.into()), primitives);
            }
        }
    }
}

fn syntax_of<'db, N: ast::AstNode<'db>>(source: Option<InFile<N>>) -> Option<InFile<ast::Node<'db>>> {
    source.map(|it| it.map(|it| it.syntax()))
}

fn assoc_syntax<'db>(cx: &Cx<'_, 'db>, assoc: hir::AssocItem) -> Option<InFile<ast::Node<'db>>> {
    match assoc {
        hir::AssocItem::Function(it) => syntax_of(cx.vsema.source(it)),
        hir::AssocItem::Const(it) => syntax_of(cx.vsema.source(it)),
        hir::AssocItem::TypeAlias(it) => syntax_of(cx.vsema.source(it)),
    }
}

/// Each path of a type or trait and each lifetime of the signature, the items, bodies,
/// attributes and constants inside it left out, and every part its `cfg`s remove (a variant,
/// a field, a parameter, a generic parameter), as rustc resolves only what configuration
/// keeps.
fn walk<'db>(cx: &Cx<'_, 'db>, signature: &Signature<'db>, found: &mut Vec<Found>) {
    let mut walk = signature.node.value.preorder();
    walk.next();
    while let Some(event) = walk.next() {
        let WalkEvent::Enter(node) = event else { continue };
        if !cx.cfg_enabled(node) {
            walk.skip_subtree();
            continue;
        }
        match node.kind() {
            SyntaxKind::PATH_TYPE => {
                let ty = ast::PathType::cast(node).expect("a PATH_TYPE node is a path type");
                if let Some(path) = ty.path() {
                    found.extend(paths::check(cx, signature, ty, path));
                }
            }
            SyntaxKind::LIFETIME => {
                let lifetime = ast::Lifetime::cast(node).expect("a LIFETIME node is a lifetime");
                found.extend(lifetimes::check(cx, signature, lifetime));
            }
            _ if is_skipped(node) => walk.skip_subtree(),
            _ => {}
        }
    }
}

fn is_skipped(node: ast::Node<'_>) -> bool {
    let kind = node.kind();
    ast::Item::can_cast(kind)
        || ast::AssocItemList::can_cast(kind)
        || ast::Expr::can_cast(kind)
        || ast::Attr::can_cast(kind)
        || ast::ConstArg::can_cast(kind)
        || ast::Visibility::can_cast(kind)
        || ast::MacroCall::can_cast(kind)
        || ast::UseBoundGenericArgs::can_cast(kind)
}
