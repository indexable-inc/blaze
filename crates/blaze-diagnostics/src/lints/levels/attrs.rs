//! Where rustc's walk of the crate comes to a node with lint attributes, for the order of what
//! is said of them, and whether a span is in an external macro. How the attributes are read
//! is the front end's: `hir_lint_levels::attrs`.
//!
//! Follows rustc's `compiler/rustc_span/src/lib.rs` (`Span::in_external_macro`) and
//! `compiler/rustc_expand/src/expand.rs` (`InvocationCollector`: a derive's output is beside
//! the item, an attribute macro's in its place) (MIT or Apache-2.0).

use hir::{InFile, view::Semantics};
use hir_expand::MacroCallKind;
use syntax::{
    TextRange,
    view::{self as ast, AstNode as _},
};

use crate::model::{MacroKind, Span};

/// Where rustc's walk of the crate comes to `node`: the places of the `mod` items and macro
/// calls it is reached through, outermost first, then its own. What a derive makes comes after
/// the item it is on.
pub(super) fn place(sema: &Semantics<'_>, node: InFile<ast::Node<'_>>) -> Vec<(u32, u32)> {
    // alloc: made only for a diagnostic's place, or once per `#[expect]` name
    let mut place = vec![entered(node.value.text_range())];
    let mut file = node.file_id;
    loop {
        if let Some(call) = file.macro_file() {
            let loc = call.loc(sema.db);
            let at = loc.to_node_view(sema.db);
            place.push(match loc.kind {
                MacroCallKind::Derive { .. } => {
                    let item = at.value.ancestors().find(|it| ast::Adt::can_cast(it.kind()));
                    let end = item.map_or(at.value.text_range(), |it| it.text_range()).end();
                    (u32::from(end), 1)
                }
                _ => entered(at.value.text_range()),
            });
            file = at.file_id;
            continue;
        }
        match sema.find_parent_file(file) {
            Some(declared) => {
                place.push(entered(declared.value.text_range()));
                file = declared.file_id;
            }
            None => break,
        }
    }
    place.reverse();
    place
}

/// The place of entering a node of `range`: outer nodes before the nodes in them.
fn entered(range: TextRange) -> (u32, u32) {
    (u32::from(range.start()), u32::MAX - u32::from(range.end()))
}

/// rustc's `Span::in_external_macro`: made by an attribute or derive macro, or by a macro
/// of another crate, rather than handed to it. [`crate::cx::Cx::span`] puts a span a macro of
/// another crate makes where the macro is called, as rustc writes it, and one of this
/// crate's macros where the macro has it.
pub(super) fn external(span: &Span) -> bool {
    let Some(expansion) = &span.expansion else { return false };
    match expansion.kind {
        MacroKind::Attr | MacroKind::Derive => true,
        MacroKind::Bang => {
            let call = &expansion.call_site;
            span.file == call.file && span.lo == call.lo && span.hi == call.hi
        }
    }
}
