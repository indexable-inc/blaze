//! The hints of a packed struct or union that contradict it: E0634 for a `packed` of another
//! alignment than the type is packed to (the least of its `packed`s), E0587 for `packed` with
//! `align`, both at the type; and the lint `aligned_fields_in_packed` (deny by default) for a
//! `repr(C, packed)` type that holds, through `repr(C)` structs and unions, a `repr(C,
//! align)` one: a lint since Rust 1.100 (rust-lang/rust#162160), E0588 before it, whatever
//! the reprs.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/check.rs` (`check_packed`,
//! `check_packed_inner`) (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir::FieldSourceView;
use hir_def::attrs::AttrFlags;
use syntax::{
    NodeOrToken, SyntaxKind,
    view::{self as ast, AstNode as _, AstToken as _, HasAttrs as _, HasName as _},
};

use crate::{
    cx::{Cx, def_range_view as def_range},
    lints::registry::LintId,
    lower::{Found, Phase},
    model::Diag,
};

const ALIGNED_FIELDS_IN_PACKED: LintId = LintId::named("aligned_fields_in_packed");

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
            let mut diag = Diag::error(
                Some("E0634"),
                "type has conflicting packed representation hints",
                span(),
            );
            diag.fact(fact(), what());
            found.push(Found::new(diag, Phase::Items));
        }
    }
    if repr.align.is_some() {
        let mut diag = Diag::error(
            Some("E0587"),
            "type has conflicting packed and align representation hints",
            span(),
        );
        diag.fact(fact(), what());
        found.push(Found::new(diag, Phase::Items));
    } else if repr.c()
        && cx.lint_may_fire(ALIGNED_FIELDS_IN_PACKED)
        // alloc: the stack of the types on the way, empty for a type with no ADT field
        && let Some(chain) = aligned_inside(cx, adt, &mut Vec::new())
        && let Some(diag) = aligned_in_packed(cx, adt, &chain)
    {
        let at = node.map(|it| ast::ptr(it));
        found.push(Found::lint(diag, Phase::Items, at));
    }
    found
}

/// rustc's `check_packed_inner`: the `repr(C, align)` struct or union `adt` is or holds,
/// through fields of `repr(C)` structs and unions: that type, then each type on the way out
/// to `adt` with the field it is held by, innermost first.
fn aligned_inside(
    cx: &Cx<'_, '_>,
    adt: hir::Adt,
    stack: &mut Vec<hir::Adt>,
) -> Option<Vec<(hir::Adt, Option<hir::Field>)>> {
    let fields = match adt {
        hir::Adt::Struct(it) => it.fields(cx.db),
        hir::Adt::Union(it) => it.fields(cx.db),
        hir::Adt::Enum(_) => return None,
    };
    let repr = AttrFlags::repr(cx.db, hir_def::AdtId::from(adt))?;
    if !repr.c() {
        return None;
    }
    if repr.align.is_some() {
        // alloc: the lint's chain, for a type that is linted
        return Some(vec![(adt, None)]);
    }
    stack.push(adt);
    for field in fields {
        if let Some(inner) = field.ty(cx.db).as_adt()
            && !stack.contains(&inner)
            && let Some(mut chain) = aligned_inside(cx, inner, stack)
        {
            chain.push((inner, Some(field)));
            return Some(chain);
        }
    }
    stack.pop();
    None
}

/// The lint at the packed `adt`, with rustc's notes: the aligned type, then, when it is not
/// a field of `adt` itself, each field on the way to it.
fn aligned_in_packed(
    cx: &Cx<'_, '_>,
    adt: hir::Adt,
    chain: &[(hir::Adt, Option<hir::Field>)],
) -> Option<Diag> {
    let name = |adt: hir::Adt| adt.name(cx.db).as_str().to_owned();
    let def_span = |adt: hir::Adt| super::recursive::def_span(cx, adt);
    // rustc's `field.ident(tcx).span`: a named field's name, a positional one's type.
    let field_span = |field: hir::Field| {
        let source = cx.vsema.source(field)?;
        let range = match source.value {
            FieldSourceView::Named(it) => it.name()?.syntax().text_range(),
            FieldSourceView::Pos(it) => it.ty()?.syntax().text_range(),
        };
        Some(cx.span(source.with_value(range)))
    };
    let (aligned, _) = *chain.first()?;
    let mut diag = Diag::lint(
        "aligned_fields_in_packed",
        "packed type cannot transitively contain a `#[repr(align)]` type",
        def_span(adt)?,
    );
    // alloc: the lint's notes, from here on
    diag.span_note(
        def_span(aligned)?,
        format!("`{}` has a `#[repr(align)]` attribute", name(aligned)),
    );
    // Two types mean the aligned one is a field of the packed one: nothing more to say.
    if chain.len() > 2 {
        let mut first = true;
        for &(held, field) in chain[1..].iter().rev() {
            let message = match first {
                true => format!("`{}` contains a field of type `{}`", name(adt), name(held)),
                false => format!("...which contains a field of type `{}`", name(held)),
            };
            diag.span_note(field_span(field?)?, message);
            first = false;
        }
    }
    Some(diag)
}

/// The alignment of each `packed` hint of a `#[repr(..)]`: 1 for `packed`, `N` for
/// `packed(N)`, `None` for one whose argument is no integer (its parser reports it).
fn packs(attr: ast::Attr<'_>) -> impl Iterator<Item = Option<u64>> + '_ {
    let hints = attr
        .meta()
        .and_then(|it| it.as_simple_call())
        .map(|(_, tt)| tt.syntax().children_with_tokens());
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
