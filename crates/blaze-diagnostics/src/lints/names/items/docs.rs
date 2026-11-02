//! `missing_docs`: an exported item, associated item, foreign item, variant or named field
//! with no doc comment, no `#[doc = ..]` and no `#[doc(hidden)]`, and the crate with none.
//! Impls, `use`s and `extern crate`s are not linted, nor the items of a trait impl or of an
//! inherent impl of a `#[doc(hidden)]` type.
//!
//! Follows `compiler/rustc_lint/src/builtin.rs` (`MissingDoc`, `has_doc`) and
//! `compiler/rustc_middle/src/ty/mod.rs` (`article_and_description`, `def_descr`) (MIT or
//! Apache-2.0).

use blaze_items::Fact;
use hir::{InFile, view::ModuleSourceView};
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize, T,
    view::{self as ast, AstNode, HasAttrs},
};

use super::{
    def_span, visibility,
    walk::{self, Container, Item},
};
use crate::lints::names::effective::{Def, Effective};
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

/// `check_crate`: the crate's documentation.
pub(super) fn krate(cx: &Cx<'_, '_>) -> Vec<Found> {
    let root = cx.krate.root_module(cx.db);
    let source = root.definition_source_view(cx.db);
    let ModuleSourceView::SourceFile(_) = source.value else { return Vec::new() }; // alloc: empty, allocates nothing
    let file = source.file_id;
    // alloc: empty, allocates nothing
    let Some(source_file) = ast::SourceFile::cast(cx.root(file)) else { return Vec::new() };
    if has_doc(cx, source_file, Some(source_file.syntax()), None) {
        return Vec::new(); // alloc: empty, allocates nothing
    }
    let Some(range) = tokens_range(source_file.syntax()) else { return Vec::new() }; // alloc: empty, allocates nothing
    let mut diag = Diag::lint("missing_docs", "missing documentation for the crate", cx.span(InFile::new(file, range)));
    diag.fact(Fact::Name(cx.module_key(root)), "the attributes of the crate");
    vec![Found::lint(diag, Phase::Late, InFile::new(file, ast::ptr(source_file.syntax())))] // alloc: the lint that fires
}

/// `check_item`, `check_trait_item`, `check_impl_item`, `check_foreign_item`, then the
/// variants and fields among the item's own nodes.
pub(super) fn item<'db>(cx: &Cx<'_, 'db>, item: &Item<'db>, effective: &Effective) -> Vec<Found> {
    let mut found = Vec::new(); // alloc: empty until a lint fires; holds only what fires
    let node = item.node.value;
    if matches!(node, ast::Item::Impl(_) | ast::Item::Use(_) | ast::Item::ExternCrate(_)) {
        return found;
    }
    if let Container::Impl(imp) = item.container
        && imp.value.trait_().is_some()
    {
        return found;
    }
    // Asked last: only an exported item with no docs of its own needs it.
    let hidden_impl = || matches!(item.container, Container::Impl(imp) if hidden_self_ty(cx, imp));
    let Some(desc) = describe(item) else { return found };
    let exported = match node {
        ast::Item::MacroRules(it) => walk::attr(cx, it, "macro_export").is_some(),
        ast::Item::ExternBlock(_) => false,
        _ => visibility::def(cx, item).is_some_and(|def| effective.is_exported(def)),
    };
    let list = match node {
        ast::Item::Module(it) => it.item_list(),
        _ => None,
    };
    if exported && !has_doc(cx, node, list.map(|it| it.syntax()), list) && !hidden_impl() {
        found.push(lint(cx, item.syntax(), desc));
    }
    if !matches!(node, ast::Item::Struct(_) | ast::Item::Union(_) | ast::Item::Enum(_)) {
        return found;
    }
    for &own in &item.nodes()[1..] {
        match own.value.kind() {
            // A variant `pub use` names is exported without its enum.
            SyntaxKind::VARIANT => {
                let Some(variant) = ast::Variant::cast(own.value) else { continue };
                let def = cx.vsema.to_def2(own.with_value(variant));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Item(hir::ModuleDef::EnumVariant(it))));
                if exported && !has_doc(cx, variant, None, None) {
                    found.push(lint(cx, own, ("a", "variant")));
                }
            }
            // A field is exported only with the type it is of.
            SyntaxKind::RECORD_FIELD if exported => {
                let Some(field) = ast::RecordField::cast(own.value) else { continue };
                let def = cx.vsema.to_def2(own.with_value(field));
                let exported = def.is_some_and(|it| effective.is_exported(Def::Field(it)));
                if exported && !has_doc(cx, field, None, None) {
                    found.push(lint(cx, own, ("a", "struct field")));
                }
            }
            _ => {}
        }
    }
    found
}

fn lint(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>, (article, desc): (&str, &str)) -> Found {
    let span = cx.span(node.with_value(def_span(node.value)));
    // alloc: the firing lint's message
    let mut diag = Diag::lint("missing_docs", format!("missing documentation for {article} {desc}"), span);
    let at = node.map(ast::ptr);
    crate::lints::code_fact(cx, &mut diag, at);
    Found::lint(diag, Phase::Late, at)
}

/// rustc's `has_doc`: a doc comment or `#[doc = ..]` on `owner` (or, inner, in `inner`, and
/// as attributes of `list`, the module's item list `inner` is), or a `#[doc(hidden)]`.
fn has_doc<'t>(cx: &Cx<'_, '_>, owner: impl HasAttrs<'t>, inner: Option<ast::Node<'t>>, list: Option<ast::ItemList<'t>>) -> bool {
    let outer_comment = owner.syntax().children().any(|it| is_doc_comment(it, T![outer_doc_comment]));
    let inner_comment = inner.is_some_and(|it| it.children().any(|it| is_doc_comment(it, T![inner_doc_comment])));
    if outer_comment || inner_comment {
        return true;
    }
    let documented = |meta: ast::Meta<'t>| match meta {
        ast::Meta::KeyValueMeta(_) => true,
        ast::Meta::TokenTreeMeta(it) => it.token_tree().is_some_and(|tt| {
            tt.syntax()
                .children_with_tokens()
                .filter_map(NodeOrToken::into_token)
                .any(|token| token.kind() == SyntaxKind::IDENT && token.text() == "hidden")
        }),
        _ => false,
    };
    if walk::any_named(cx, owner, "doc", documented) {
        return true;
    }
    list.is_some_and(|list| walk::any_named(cx, list, "doc", documented))
}

fn is_doc_comment(node: ast::Node<'_>, kind: SyntaxKind) -> bool {
    node.kind() == SyntaxKind::DOC_COMMENT
        && node.children_with_tokens().any(|it| it.kind() == kind)
}

/// From the first token of a file to the end of its last: the span of the crate.
fn tokens_range(node: ast::Node<'_>) -> Option<TextRange> {
    let mut tokens = node.descendants_with_tokens().filter_map(NodeOrToken::into_token).filter(|it| !it.kind().is_trivia());
    let first: TextSize = tokens.next()?.text_range().start();
    let last = node
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .filter(|it| !it.kind().is_trivia())
        .last()
        .map_or(first, |it| it.text_range().end());
    Some(TextRange::new(first, last))
}

/// Whether the self type of an inherent impl is a `#[doc(hidden)]` type.
fn hidden_self_ty<'db>(cx: &Cx<'_, 'db>, imp: InFile<ast::Impl<'db>>) -> bool {
    let Some(def) = cx.vsema.to_def2(imp) else { return false };
    let Some(adt) = def.self_ty(cx.db).as_adt() else { return false };
    hir::HasAttrs::attrs(adt, cx.db).is_doc_hidden()
}

/// rustc's `article_and_description` of an item.
fn describe(item: &Item<'_>) -> Option<(&'static str, &'static str)> {
    let assoc = matches!(item.container, Container::Trait(_) | Container::Impl(_));
    Some(match &item.node.value {
        ast::Item::Fn(it) if assoc => match it.param_list().and_then(|it| it.self_param()) {
            Some(_) => ("a", "method"),
            None => ("an", "associated function"),
        },
        ast::Item::Fn(_) => ("a", "function"),
        ast::Item::Const(_) if assoc => ("an", "associated constant"),
        ast::Item::Const(_) => ("a", "constant"),
        ast::Item::TypeAlias(_) if assoc => ("an", "associated type"),
        ast::Item::TypeAlias(_) if matches!(item.container, Container::Extern(_)) => ("a", "foreign type"),
        ast::Item::TypeAlias(_) => ("a", "type alias"),
        ast::Item::Static(_) => ("a", "static"),
        ast::Item::Struct(_) => ("a", "struct"),
        ast::Item::Enum(_) => ("an", "enum"),
        ast::Item::Union(_) => ("a", "union"),
        ast::Item::Trait(it) if it.eq_token().is_some() => ("a", "trait alias"),
        ast::Item::Trait(_) => ("a", "trait"),
        ast::Item::Module(_) => ("a", "module"),
        ast::Item::MacroRules(_) => ("a", "macro"),
        ast::Item::MacroDef(_) => ("a", "macro"),
        _ => return None,
    })
}
