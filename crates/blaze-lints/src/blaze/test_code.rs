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

/// [`is_test_code`] for a body by its id, where no tree is walked (a crate-wide scan): in a
/// test, bench or example target, a `#[test]` function, or under a module whose own attributes
/// say `cfg(test)`. A `#[cfg(test)]` on an item inside a module that is not one is not seen
/// here; [`is_test_code`] sees it in the tree.
pub(super) fn is_test_def(db: &base_db::Db, def: hir_def::DefWithBodyId) -> bool {
    let body = hir::DefWithBody::from(def);
    let mut module = Some(body.module(db));
    if module.is_some_and(|it| {
        matches!(it.krate(db).target_kind(db), TargetKind::Test | TargetKind::Bench | TargetKind::Example)
    }) {
        return true;
    }
    if let hir::DefWithBody::Function(it) = body
        && it.is_test(db)
    {
        return true;
    }
    while let Some(it) = module.take() {
        if it.attrs(db).cfgs(db).is_some_and(names_test) {
            return true;
        }
        module = it.parent(db);
    }
    false
}

/// The module `node` is in: its innermost item's, or, for the file's own inner attributes, the
/// module of the file's first item. A macro call is an `Item` to the grammar but defines
/// nothing, so it is passed over: a `println!` in a body is in its function's module.
pub(super) fn module_of<'t>(cx: &Cx<'_, 't>, node: ast::Node<'t>) -> Option<hir::Module> {
    let db = cx.db();
    let is_item = |it: &ast::Node<'t>| {
        ast::Item::cast(*it).is_some_and(|it| !matches!(it, ast::Item::MacroCall(_)))
    };
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
/// the word `test`. The one home of the rule: `test_misses_its_subject` asks it too.
pub(super) fn names_test(cfg: &CfgExpr) -> bool {
    let is_test =
        |it: &CfgExpr| matches!(it, CfgExpr::Atom(CfgAtom::Flag(it)) if *it == intern::sym::test);
    match cfg {
        CfgExpr::All(items) => items.iter().any(is_test),
        it => is_test(it),
    }
}
