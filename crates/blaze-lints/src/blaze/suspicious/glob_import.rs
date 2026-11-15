//! `glob_import`: a private `use path::*;`. Every public name of `path` comes into scope
//! unseen: a reader of a name in the file cannot tell where it is from without resolving it,
//! an item added to `path` later can shadow a name or make a call resolve to another method,
//! and an edit that searches the file for an import finds none. Clippy's `wildcard_imports`
//! says the same, but `blaze check` does not run it, so this is its blaze form with blaze's
//! exemptions; the workspace's `clippy::wildcard_imports` stays off.
//!
//! Not checked: a `pub use` (a re-export is the facade a glob is for), a glob of a module named
//! `prelude` (made to be globbed), a `use super::*;` in test code (a test module is its parent's
//! inside), and a glob of an enum's variants inside a function body (a `match` over the enum
//! reads as its variants' names, and the scope is the one function).
//!
//! Fix (machine-applicable): the glob becomes the list of what the scope of the `use` names
//! through it, each resolved by name resolution (`blaze_intent`'s `Retarget::names`), with each
//! trait of the module kept as `Trait as _` (a method call names no trait). A name of the
//! module that appears only inside a macro's input is kept too, and makes the fix maybe
//! incorrect: the tokens of a macro call are not resolved here.

use std::alloc::Global;

use blaze_intent::retarget::Retarget;
use hir::ModuleDef;
use syntax::{
    SyntaxKind,
    view::{self as ast, AstNode as _, HasVisibility as _},
};

use super::super::{
    Lint,
    std_mistakes::{Fix, report},
};
use crate::{check::Cx, diag::Applicability, lints::disallowed::names};

const LINT: Lint = Lint::GlobImport;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if node.kind() != SyntaxKind::USE_TREE || !cx.on_blaze(LINT) || cx.analysis().is_some() {
        return;
    }
    let Some(tree) = ast::UseTree::cast(node) else { return };
    let Some(star) = tree.star_token() else { return };
    let Some(item) = node.ancestors().find_map(ast::Use::cast) else { return };
    if item.visibility().is_some() {
        return;
    }
    let Some(path) = tree.path() else { return };
    let db = cx.db();
    let target = match cx.path(path) {
        Some(hir::PathResolution::Def(it)) => it,
        _ => return,
    };
    let in_fn = item.syntax().parent().is_some_and(|it| it.kind() == SyntaxKind::STMT_LIST);
    let written = cx.source_of(path.syntax()).into_owned();
    match target {
        ModuleDef::Module(module) => {
            if module.name(db).is_some_and(|it| it.as_str().contains("prelude")) {
                return;
            }
            let is_super = path.qualifier().is_none()
                && path.segment().is_some_and(|it| it.super_token().is_some());
            if is_super && cx.is_in_test(node) {
                return;
            }
        }
        ModuleDef::Adt(hir::Adt::Enum(_)) if in_fn => return,
        ModuleDef::Adt(hir::Adt::Enum(_)) => {}
        _ => return,
    }
    let Some(range) = cx.range(node) else { return };
    let scope = item.syntax().parent().unwrap_or(cx.root());
    let fix = list(cx, target, scope).map(|(names, applicability)| Fix {
        message: "import each name the scope uses",
        parts: vec![(star.text_range(), names)],
        applicability,
    });
    let message = format!(
        "`use {written}::*` brings every public name of `{written}` into scope unseen: a name here \
         does not say where it is from, and an item added to `{written}` can shadow one silently"
    );
    report(
        cx,
        LINT,
        node,
        range,
        message,
        "import the names the scope uses, each by name (`use a::{B, C};`); a trait used only for \
         its methods is `Trait as _`",
        fix,
    );
}

/// The names `scope` uses through a glob of `target`, as the text the `*` becomes, and how sure
/// it is. `None` when the scope names nothing through it: the `use` is then unused, for rustc's
/// `unused_imports` to say.
fn list<'t>(
    cx: &Cx<'_, 't>,
    target: ModuleDef,
    scope: ast::Node<'t>,
) -> Option<(String, Applicability)> {
    let db = cx.db();
    let resolve = |path: ast::Path<'t>| names(cx, path);
    let to = |_: ModuleDef| None;
    let carries = |_: &str, _: ModuleDef| false;
    let retarget = Retarget { db, resolve: &resolve, to: &to, carries: &carries };
    let mut entries: Vec<(String, ModuleDef), Global> = Vec::new_in(Global);
    match target {
        ModuleDef::Module(module) => {
            for (name, def) in module.scope(db, None) {
                if let hir::ScopeDef::ModuleDef(def) = def {
                    entries.push((name.as_str().to_owned(), def));
                }
            }
        }
        ModuleDef::Adt(hir::Adt::Enum(it)) => {
            for variant in it.variants(db) {
                entries.push((variant.name(db).as_str().to_owned(), ModuleDef::from(variant)));
            }
        }
        _ => return None,
    }
    let in_macros = macro_idents(scope);
    let mut applicability = Applicability::MachineApplicable;
    let mut used: Vec<String, Global> = Vec::new_in(Global);
    for (name, def) in entries {
        if retarget.names(scope, &name, def) {
            used.push(name);
        } else if matches!(def, ModuleDef::Trait(_)) {
            used.push(format!("{name} as _"));
        } else if in_macros.iter().any(|it| *it == name) {
            applicability = Applicability::MaybeIncorrect;
            used.push(name);
        }
    }
    if used.is_empty() {
        return None;
    }
    used.sort_unstable();
    used.dedup();
    let text = match used.as_slice() {
        [one] if !one.ends_with(" as _") => one.clone(),
        _ => format!("{{{}}}", used.join(", ")),
    };
    Some((text, applicability))
}

/// The identifiers inside the macro calls of `scope`, outside modules within it: what a glob
/// may bring that no path of the tree shows.
fn macro_idents(scope: ast::Node<'_>) -> Vec<&str, Global> {
    let mut out: Vec<&str, Global> = Vec::new_in(Global);
    for node in scope.descendants() {
        if node.kind() != SyntaxKind::TOKEN_TREE {
            continue;
        }
        for token in node.descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.kind() == SyntaxKind::IDENT {
                out.push(token.text());
            }
        }
    }
    out
}

