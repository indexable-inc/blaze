//! A segment of a signature's path naming an item the place can't see: E0603 "{descr} `x` is
//! private", with the item's head. Only items declared in the module the segment is looked up
//! in; one reached through a private import is rustc's import chain, which needs hir to say
//! which names of a scope are imports.
//!
//! Follows rustc's `compiler/rustc_resolve/src/diagnostics/impls.rs` (`report_privacy_error`)
//! and `diagnostics/mod.rs` (`IsPrivate`, `NoteAndRefersToTheItemDefinedHere`) (MIT or
//! Apache-2.0).

use hir::{HasVisibility as _, InFile, ModuleDef, PathResolution, Visibility};
use syntax::view as ast;

use super::{Signature, failure::Segment};
use crate::{
    cx::Cx,
    lower::unresolved_ident::{Ns, def_descr, def_span},
    model::Diag,
};

/// The first segment after the first whose item the module the path is in (`current`, asked
/// for only when an item is not `pub`) can't see: its index among `prefixes`, its item, and
/// that module. `prefixes` are the path's prefixes, outermost first; `defs` what all but the
/// last were resolved to, which are not resolved again.
pub(super) fn private<'db>(
    cx: &Cx<'_, 'db>,
    prefixes: &[ast::Path<'db>],
    defs: &[ModuleDef],
    current: impl FnOnce() -> Option<hir::Module>,
) -> Option<(usize, ModuleDef, hir::Module)> {
    let mut current = Some(current);
    let mut from: Option<hir::Module> = None;
    let mut parent: Option<ModuleDef> = None;
    for (at, &prefix) in prefixes.iter().enumerate() {
        let def = match defs.get(at) {
            Some(def) => *def,
            None => match cx.vsema.resolve_path(prefix)? {
                PathResolution::Def(def) => def,
                _ => return None,
            },
        };
        if let Some(ModuleDef::Module(module)) = parent
            && def.visibility(cx.db) != Visibility::Public
        {
            let module_of_path = match from {
                Some(it) => it,
                None => *from.insert(current.take()?()?),
            };
            if !def.is_visible_from(cx.db, module_of_path) && module.declarations_iter(cx.db).any(|it| it == def) {
                return Some((at, def, module_of_path));
            }
        }
        parent = Some(def);
    }
    None
}

/// The E0603 of `segment`, which names `def`, an item the path's module can't see.
pub(super) fn diag(cx: &Cx<'_, '_>, signature: &Signature<'_>, segment: &Segment, def: ModuleDef) -> Option<Diag> {
    let descr = def_descr(cx, Ns::Type, def)?;
    let span = cx.span(InFile::new(signature.node.file_id, segment.range));
    // alloc: clone: the error's message and span
    let mut diag = Diag::error(Some("E0603"), format!("{descr} `{}` is private", segment.name), span.clone());
    // alloc: the error's label
    diag.span_label(span, format!("private {descr}"));
    if let Some(at) = def_span(cx, Ns::Type, def) {
        // alloc: the error's note
        diag.span_note(at, format!("the {descr} `{}` is defined here", segment.name));
    }
    Some(diag)
}
