//! The hints of a packed struct or union that contradict it: E0634 for a `packed` of another
//! alignment than the type is packed to (the least of its `packed`s), E0587 for `packed` with
//! `align`, both at the type.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/check.rs` (`check_packed`) (MIT or
//! Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir_def::attrs::AttrFlags;
use syntax::{
    NodeOrToken, SyntaxKind,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lower::{Found, Phase},
    model::Diag,
};

pub(super) fn check(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<Found> {
    let node = match adt {
        hir::Adt::Struct(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        hir::Adt::Union(it) => cx.vsema.source(it).map(|it| it.map(|it| it.syntax())),
        // alloc: empty, allocates nothing
        hir::Adt::Enum(_) => return Vec::new(),
    };
    let repr = AttrFlags::repr(cx.db, hir_def::AdtId::from(adt));
    // alloc: empty, allocates nothing
    let (Some(repr), Some(node)) = (repr, node) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(pack) = repr.pack else { return Vec::new() };
    let span = || cx.span(node.map(def_range));
    // alloc: the errors' fact
    let what = || format!("the layout of `{}`", cx.def_path(hir::ModuleDef::Adt(adt)));
    let fact = || Fact::Layout(cx.key(ItemId::Adt(adt.into())));
    // alloc: empty unless there is an error
    let mut found = Vec::new();
    let attrs = ast::AnyHasAttrs::cast(node.value).into_iter().flat_map(|it| it.attrs());
    for written in attrs.filter(|it| it.simple_name() == Some("repr")).flat_map(packs) {
        if written != Some(pack.bytes()) {
            let mut diag = Diag::error(Some("E0634"), "type has conflicting packed representation hints", span());
            diag.fact(fact(), what());
            found.push(Found::new(diag, Phase::Items));
        }
    }
    if repr.align.is_some() {
        let mut diag = Diag::error(Some("E0587"), "type has conflicting packed and align representation hints", span());
        diag.fact(fact(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// The alignment of each `packed` hint of a `#[repr(..)]`: 1 for `packed`, `N` for
/// `packed(N)`, `None` for one whose argument is no integer (its parser reports it).
fn packs(attr: ast::Attr<'_>) -> impl Iterator<Item = Option<u64>> + '_ {
    let hints = attr.meta().and_then(|it| it.as_simple_call()).map(|(_, tt)| tt.syntax().children_with_tokens());
    let mut hints = hints.into_iter().flatten().filter(|it| !it.kind().is_trivia()).peekable();
    std::iter::from_fn(move || {
        loop {
            let NodeOrToken::Token(token) = hints.next()? else { continue };
            if token.kind() != SyntaxKind::IDENT || token.text() != "packed" {
                continue;
            }
            let Some(NodeOrToken::Node(args)) = hints.peek().copied() else { return Some(Some(1)) };
            hints.next();
            let int = args.children_with_tokens().find_map(|it| match it {
                NodeOrToken::Token(it) => ast::IntNumber::cast(it),
                NodeOrToken::Node(_) => None,
            });
            return Some(int.and_then(|it| it.value().ok()).and_then(|it| u64::try_from(it).ok()));
        }
    })
}
