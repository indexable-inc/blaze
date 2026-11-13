//! Whether a node is test code, for the lints that leave tests alone (`unwired`, `retry`,
//! `narrowed_len`, `process_static`): inside a `#[test]` fn or under an item or module with
//! `#[cfg(test)]` ([`Cx::is_in_test`]), in a module whose own attributes say `cfg(test)` (a
//! `#[cfg(test)] mod tests;` brought in from its own file, which a walk of that file outside a
//! body cannot see in its tree), or anywhere in a test, bench or example target.

use base_db::TargetKind;
use cfg::{CfgAtom, CfgExpr};
use hir::HasAttrs as _;
use syntax::view::{self as ast, AstNode as _};

use crate::check::Cx;

pub(super) fn is_test_code<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> bool {
    let db = cx.db();
    if matches!(cx.krate().target_kind(db), TargetKind::Test | TargetKind::Bench | TargetKind::Example) {
        return true;
    }
    if cx.is_in_test(node) {
        return true;
    }
    let mut module = module_of(cx, node);
    while let Some(it) = module.take() {
        if it.attrs(db).cfgs(db).is_some_and(names_test) {
            return true;
        }
        module = it.parent(db);
    }
    false
}

/// The module `node` is in: its innermost item's, or, for the file's own inner attributes, the
/// module of the file's first item.
pub(super) fn module_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<hir::Module> {
    let db = cx.db();
    let is_item = |it: &ast::Node<'t>| ast::Item::cast(*it).is_some();
    let item = node
        .ancestors()
        .find(is_item)
        .or_else(|| node.ancestors().last().and_then(|root| root.children().find(is_item)))?;
    match cx.def(item)? {
        hir::ModuleDef::Module(it) => Some(it),
        it => it.module(db),
    }
}

/// clippy's `is_cfg_test`, as [`Cx::is_in_test`] reads a module's `cfg`: the `cfg`'s list has
/// the word `test`.
fn names_test(cfg: &CfgExpr) -> bool {
    let is_test =
        |it: &CfgExpr| matches!(it, CfgExpr::Atom(CfgAtom::Flag(it)) if *it == intern::sym::test);
    match cfg {
        CfgExpr::All(items) => items.iter().any(is_test),
        it => is_test(it),
    }
}
