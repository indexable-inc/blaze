//! A `use` more visible than every item it brings in: E0365 when the last of them is a type or
//! module, E0364 otherwise ("`x` is private, and cannot be re-exported", or "is only public
//! within the crate" for an item of the crate root). Single imports only: a glob re-exports
//! what it can. An item reached through another import is compared by its own visibility,
//! which that import's can only lower: those are missed, never reported wrongly (hir does not
//! say which names of a scope are imports).
//!
//! Follows rustc's `compiler/rustc_resolve/src/imports.rs` (`finalize_import`'s
//! `reexport_error`, `report_cannot_reexport`), `compiler/rustc_middle/src/ty/mod.rs`
//! (`Visibility::greater_than`) and `diagnostics/mod.rs` (`CannotBeReexported*`,
//! `ConsiderMarkingAsPub`, `ConsiderAddingMacroExport`, `ConsiderMarkingAsPubCrate`) (MIT or
//! Apache-2.0).

use hir::{HasVisibility as _, InFile, ModuleDef, PathResolution, Visibility};
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _, HasName as _, HasVisibility as _},
};

use crate::{
    checks::defs::item_id,
    cx::{Cx, item_range_view as item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

/// What an import's visibility reaches: everywhere, or the module it is restricted to.
#[derive(Clone, Copy)]
enum Reach {
    Public,
    In(hir::Module),
}

pub(super) fn check(cx: &Cx<'_, '_>, scopes: &[hir::Module]) -> Vec<Found> {
    // alloc: empty, allocates only when an import re-exports what it can't
    let mut found = Vec::new();
    // alloc: setup, reused for every `pub use`, once per check
    let mut stack = Vec::new();
    for &module in scopes {
        for decl in module.use_decls(cx.db) {
            let item = decl.source_view(cx.db);
            let Some(vis) = item.value.visibility() else { continue };
            // `vsema` resolves paths only in the trees it gave out.
            cx.root(item.file_id);
            let Some(reach) = reach(cx, vis) else { continue };
            let Some(tree) = item.value.use_tree() else { continue };
            stack.push(tree);
            while let Some(tree) = stack.pop() {
                if let Some(list) = tree.use_tree_list() {
                    let start = stack.len();
                    stack.extend(list.use_trees());
                    stack[start..].reverse();
                    continue;
                }
                if tree.star_token().is_some() {
                    continue;
                }
                found.extend(leaf(cx, item.with_value(tree), vis, reach));
            }
        }
    }
    found
}

/// rustc's `Visibility` of an import's `pub(..)`, resolved where the import is.
fn reach<'db>(cx: &Cx<'_, 'db>, vis: ast::Visibility<'db>) -> Option<Reach> {
    let Some(inner) = vis.visibility_inner() else { return Some(Reach::Public) };
    match cx.vsema.resolve_path(inner.path()?)? {
        PathResolution::Def(ModuleDef::Module(it)) => Some(Reach::In(it)),
        _ => None,
    }
}

/// `import.vis.greater_than(binding.vis())`: whether the import reaches a module the item is
/// not visible from.
fn greater(cx: &Cx<'_, '_>, reach: Reach, def: ModuleDef) -> bool {
    match reach {
        Reach::Public => !matches!(def.visibility(cx.db), Visibility::Public),
        Reach::In(module) => !def.is_visible_from(cx.db, module),
    }
}

fn leaf<'db>(
    cx: &Cx<'_, 'db>,
    tree: InFile<ast::UseTree<'db>>,
    vis: ast::Visibility<'db>,
    reach: Reach,
) -> Option<Found> {
    let path = tree.value.path()?;
    let segment = path.segment()?;
    let is_self = segment.self_token().is_some() && path.qualifier().is_none();
    // rustc names the import's source, not what it is renamed to.
    let ident = if is_self {
        let parent = tree.value.syntax().ancestors().skip(1).find_map(ast::UseTree::cast)?;
        parent.path()?.segment()?.name_ref()?
    } else {
        segment.name_ref()?
    };
    let resolved = cx.vsema.resolve_path_per_ns(path)?;
    let namespaces = [
        (Ns::Type, resolved.type_ns),
        (Ns::Value, if is_self { None } else { resolved.value_ns }),
        (Ns::Macro, if is_self { None } else { resolved.macro_ns }),
    ];
    let mut error: Option<(Ns, ModuleDef)> = None;
    for (ns, res) in namespaces {
        let Some(PathResolution::Def(def)) = res else { continue };
        if !greater(cx, reach, def) {
            return None;
        }
        error = Some((ns, def));
    }
    let (ns, def) = error?;
    let ident = ident.text();

    // `Visibility::Restricted` to the crate root: what the whole crate, and only it, sees.
    let crate_private = !matches!(def.visibility(cx.db), Visibility::Public)
        && def.is_visible_from(cx.db, cx.krate.root_module(cx.db));
    let span = cx.span(tree.map(|it| it.syntax().text_range()));
    let message = match crate_private {
        // alloc: the error's message
        true => format!("`{ident}` is only public within the crate, and cannot be re-exported outside"),
        // alloc: the error's message
        false => format!("`{ident}` is private, and cannot be re-exported"),
    };
    let mut diag;
    if ns == Ns::Type {
        // clone: the error's span
        diag = Diag::error(Some("E0365"), message, span.clone());
        let label = match crate_private {
            // alloc: the error's label
            true => format!("re-export of crate public `{ident}`"),
            // alloc: the error's label
            false => format!("re-export of private `{ident}`"),
        };
        diag.span_label(span, label);
        // alloc: the error's note
        diag.note(format!("consider declaring type or module `{ident}` with `pub`"));
    } else {
        // clone: the error's span
        diag = Diag::error(Some("E0364"), message, span.clone());
        match macro_rules(cx, def) {
            Some(range) => {
                diag.span_help(cx.span(range), "consider adding a `#[macro_export]` to the macro in the imported module");
                diag.suggestion(
                    cx.span(tree.with_value(vis.syntax().text_range())),
                    "in case you want to use the macro within this crate only, reduce the visibility to `pub(crate)`",
                    "pub(crate)",
                    Applicability::MaybeIncorrect,
                );
            }
            None => {
                // alloc: the error's note
                diag.span_note(span, format!("consider marking `{ident}` as `pub` in the imported module"));
            }
        }
    }
    if let Some(item) = item_id(cx.db, def) {
        diag.fact(blaze_items::Fact::Signature(cx.key(item)), "the visibility of the re-exported item");
    }
    let module = cx.vsema.scope(path.syntax()).map(|it| it.module());
    if let Some(module) = module {
        // alloc: the error's fact
        diag.fact(cx.resolves_fact(module, ident), format!("what `{ident}` names in its scope"));
    }
    Some(Found::new(diag, Phase::Resolve))
}

/// The namespaces of an import, as rustc's `per_ns` visits them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ns {
    Type,
    Value,
    Macro,
}

/// The span of a `macro_rules!` item rustc's `ConsiderAddingMacroExport` points at.
fn macro_rules(cx: &Cx<'_, '_>, def: ModuleDef) -> Option<InFile<TextRange>> {
    let ModuleDef::Macro(it) = def else { return None };
    let source = cx.vsema.source(it)?;
    match source.value {
        either::Either::Left(ast::Macro::MacroRules(rules)) => {
            rules.name()?;
            Some(source.with_value(item_range(rules.syntax())))
        }
        _ => None,
    }
}
