//! The lints of `macro_rules!`, of expansion and of the builtin macros:
//!
//! - `semicolon_in_expressions_from_macros` and `semicolon_in_expressions_from_non_local_macros`:
//!   a `macro_rules!` macro called as an expression whose expansion ends in `;`.
//! - `incomplete_include`: an `include!` as an expression of a file with more than one.
//! - `duplicate_macro_attributes`: `#[test]`, `#[bench]`, `#[test_case]` or `#[cfg_eval]` on an
//!   item that has it again.
//! - `matchers`: what rustc lints on a macro's definition (`meta_variable_misuse`,
//!   `rust_2021_incompatible_or_patterns`, `edition_2024_expr_fragment_specifier`).
//! - `asm`: `bad_asm_style` (`named_asm_labels` and `binary_asm_labels` are the late pass
//!   `AsmLabels`, in `lints/bodies/calls/methods`).
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`ParserAnyMacro::make`,
//! `is_defined_in_current_crate`), `compiler/rustc_expand/src/expand.rs` (`assign_id!`, the
//! `lint_node_id` a lint of expansion is buffered at; `flat_map_stmt`'s `is_trailing_mac`),
//! `compiler/rustc_parse/src/parser/stmt.rs` (`parse_stmt_mac`: which macro calls in a block
//! are expressions), `compiler/rustc_builtin_macros/src/source_util.rs` (`expand_include`) and
//! `compiler/rustc_builtin_macros/src/util.rs` (`warn_on_duplicate_attribute`), with the text
//! of `compiler/rustc_expand/src/diagnostics.rs` (`TrailingMacro`) and
//! `compiler/rustc_builtin_macros/src/diagnostics.rs` (`IncompleteInclude`,
//! `DuplicateMacroAttribute`) (MIT or Apache-2.0).

mod asm;
mod matchers;

use cfg::CfgAtom;
use hir::{InFile, MacroCallId, ModuleDef, PathResolution};
use hir_def::MacroId;
use hir_expand::{ExpandTo, MacroCallKind, MacroCallLoc, MacroDefKind, builtin::BuiltinAttrExpander};
use intern::sym;
use syntax::{
    NodeOrToken, SyntaxKind, SyntaxNodePtr, T, TextRange, TextSize,
    view::{self as ast, AstNode as _, HasName as _},
};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::Phase,
    model::{Diag, Span},
};

/// The lints of macros and their expansions in the crate, as rustc's expansion buffers them,
/// and those of its early and late lint passes the parts give: one pass over the trees of
/// expansions, each call's location read once and its lints picked by the kind of its macro.
/// Macro definitions are linted in the early pass's one walk ([`definition`]).
pub(crate) fn lints<'db>(cx: &Cx<'_, 'db>, walk: &Walk<'db>) -> Vec<Emitted> {
    // alloc: empty until a lint fires; holds only what fires
    let mut emitted = Vec::new();
    let x86 = asm::x86(walk);
    for tree in &walk.trees {
        let Some(call) = tree.call else { continue };
        let loc = call.loc(cx.db);
        let found = match loc.def.kind {
            MacroDefKind::Declarative(..) => trailing_semicolon(cx, tree, call, loc),
            MacroDefKind::BuiltInEager(..) => incomplete_include(cx, tree, call, loc),
            MacroDefKind::BuiltInAttr(..) => duplicate_attribute(cx, walk, loc),
            MacroDefKind::BuiltIn(_, it) if x86 && it.is_asm() => {
                asm::lints(cx, tree, call, &mut emitted);
                None
            }
            _ => None,
        };
        emitted.extend(found);
    }
    if walk.cfg.check_atom(&CfgAtom::Flag(sym::test)) {
        for tree in &walk.trees {
            for node in tree.with_attrs(ast::Item::can_cast) {
                emitted.extend(unexpanded_test(cx, walk, tree, node));
            }
        }
    }
    emitted
}

/// The lints of a macro definition (`matchers`), which the early pass's one walk
/// (`super::lints`) gives as it comes to one; `krate` is [`crate_at`].
pub(super) fn definition(
    cx: &Cx<'_, '_>,
    krate: Option<InFile<SyntaxNodePtr>>,
    tree: &Tree<'_>,
    node: ast::Node<'_>,
    out: &mut Vec<Emitted>,
) {
    matchers::definition(cx, krate, tree, node, out);
}

/// Where the lints of macro definitions buffered at the crate are placed.
pub(super) fn crate_at(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Option<InFile<SyntaxNodePtr>> {
    matchers::crate_at(cx, walk)
}

/// `semicolon_in_expressions_from_macros` (`..._from_non_local_macros` for a macro of another
/// crate): rustc parses what a `macro_rules!` macro called as an expression expands to as one
/// expression, and when a `;` comes after it, buffers the lint at the `;` and drops it.
fn trailing_semicolon(cx: &Cx<'_, '_>, tree: &Tree<'_>, call: MacroCallId, loc: &MacroCallLoc) -> Option<Emitted> {
    let db = cx.db;
    let MacroDefKind::Declarative(def, _) = loc.def.kind else { return None };
    let MacroCallKind::FnLike { expand_to, .. } = &loc.kind else { return None };
    let node = loc.to_node_view(db);
    let mac = ast::MacroCall::cast(node.value)?;
    let is_trailing = is_trailing(mac);
    let semicolon = match expand_to {
        ExpandTo::Expr => {
            let expanded = call.parse_macro_expansion(db);
            if expanded.err.is_some() || !expanded.value.0.errors().is_empty() {
                return None;
            }
            // The `;` the front end drops from the end of an expression's expansion
            // (`without_last_semicolon` in `crates/hir-expand/src/lib.rs`), with its span.
            let semicolon = expanded.value.1.dropped_semicolon?;
            span_of_token(cx, call, semicolon)
        }
        // The front end expands a block's trailing macro call as statements, where rustc
        // expands it as an expression: the `;` after the first statement's expression is the
        // one rustc would see after the expression it parses.
        ExpandTo::Statements if is_trailing => {
            let first = tree.root.children().next()?;
            let stmt = ast::ExprStmt::cast(first)?;
            stmt.expr()?;
            let semicolon = stmt.semicolon_token()?;
            cx.span(InFile::new(tree.file, semicolon.text_range()))
        }
        _ => return None,
    };
    let lint = match hir::Crate::from(loc.def.krate) == cx.krate {
        true => "semicolon_in_expressions_from_macros",
        false => "semicolon_in_expressions_from_non_local_macros",
    };
    let mut diag = Diag::lint(lint, "trailing semicolon in macro used in expression position", semicolon);
    if is_trailing {
        let name = def.to_view(db).name()?.text();
        diag.note("macro invocations at the end of a block are treated as expressions");
        // alloc: the lint's text, for a lint found
        diag.note(format!(
            "to ignore the value produced by the macro, add a semicolon after the invocation of `{name}`"
        ));
    }
    let at = enclosing(cx, node);
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// Whether rustc's `flat_map_stmt` expands `call` with `is_trailing_mac`: it is the trailing
/// expression of a block, not in braces (`parse_stmt_mac` makes `m! {}` a statement).
fn is_trailing(call: ast::MacroCall<'_>) -> bool {
    let Some(expr) = call.syntax().parent().and_then(ast::MacroExpr::cast) else { return false };
    let Some(list) = expr.syntax().parent().and_then(ast::StmtList::cast) else { return false };
    let braced = call.token_tree().and_then(|it| it.left_delimiter_token()).is_some_and(|it| it.kind() == T!['{']);
    !braced && list.tail_expr().is_some_and(|it| it.syntax() == expr.syntax())
}

/// [`Cx::span`] of a token of the expansion of `call` by its span: where it is written when it
/// is of the source the call was handed; where the crate's own macro writes it, inside the
/// expansion; else the call of the other crate's macro.
fn span_of_token(cx: &Cx<'_, '_>, call: MacroCallId, token: span::Span) -> Span {
    let db = cx.db;
    let range = hir_expand::resolve_span(db, token);
    let written = cx.span_in_file(range.file_id.file_id(db), range.range);
    if token.ctx.is_root() {
        return written;
    }
    let expansion = cx.expansion_of(call).map(Box::new);
    let loc = call.loc(db);
    if hir::Crate::from(loc.def.krate) == cx.krate {
        return Span { expansion, ..written };
    }
    let at = match &expansion {
        // clone: a lint's span, for a lint found
        Some(it) => Span { expansion: None, ..it.call_site.clone() },
        None => {
            let at = loc.kind.original_call_range(db, loc.krate);
            cx.span_in_file(at.file_id.file_id(db), at.range)
        }
    };
    Span { expansion, ..at }
}

/// `incomplete_include`: rustc parses what `include!` called as an expression or a statement
/// reads as one expression (`ExpandInclude::make_expr`), and buffers the lint at the first
/// token of the file after it.
fn incomplete_include(cx: &Cx<'_, '_>, tree: &Tree<'_>, call: MacroCallId, loc: &MacroCallLoc) -> Option<Emitted> {
    let db = cx.db;
    if !loc.def.is_include() {
        return None;
    }
    let MacroCallKind::FnLike { expand_to, .. } = &loc.kind else { return None };
    let first = tree.root.children().next();
    let expr = match expand_to {
        ExpandTo::Expr if ast::Expr::can_cast(tree.root.kind()) => tree.root,
        ExpandTo::Expr => first.filter(|it| ast::Expr::can_cast(it.kind()))?,
        ExpandTo::Statements => match first? {
            it if ast::Expr::can_cast(it.kind()) => it,
            it => ast::ExprStmt::cast(it)?.expr()?.syntax(),
        },
        _ => return None,
    };
    let file = loc.include_file_id(db, call)?;
    let (read, _) = InFile::new(tree.file, expr.text_range()).original_node_file_range_opt(db)?;
    if read.file_id != file {
        return None;
    }
    let source = cx.vsema.parse(file);
    let rest = next_token(source.syntax(), read.range.end())?;
    let span = cx.span_in_file(file.file_id(db), rest.text_range());
    let mut diag = Diag::lint("incomplete_include", "include macro expected single expression in source", span);
    let at = enclosing(cx, loc.to_node_view(db));
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// The first token of `root` from `offset` that rustc's lexer gives: not white space nor a
/// comment, but a doc comment.
fn next_token(root: ast::Node<'_>, offset: TextSize) -> Option<ast::Token<'_>> {
    root.descendants_with_tokens().filter_map(NodeOrToken::into_token).find(|token| {
        if token.text_range().start() < offset {
            return false;
        }
        !token.kind().is_trivia()
    })
}

/// `duplicate_macro_attributes`: as rustc expands the builtin attribute `#[test]`, `#[bench]`,
/// `#[test_case]` or `#[cfg_eval]`, the first other attribute of that name on the item it is on
/// (`warn_on_duplicate_attribute`).
fn duplicate_attribute(cx: &Cx<'_, '_>, walk: &Walk<'_>, loc: &MacroCallLoc) -> Option<Emitted> {
    let db = cx.db;
    let MacroDefKind::BuiltInAttr(_, expander) = loc.def.kind else { return None };
    let name = match expander {
        BuiltinAttrExpander::Test => "test",
        BuiltinAttrExpander::Bench => "bench",
        BuiltinAttrExpander::TestCase => "test_case",
        BuiltinAttrExpander::CfgEval => "cfg_eval",
        _ => return None,
    };
    let MacroCallKind::Attr { ast_id, censored_attr_ids, .. } = &loc.kind else { return None };
    let node = loc.to_node_view(db);
    let (_, invoked) = censored_attr_ids.invoc_attr().find_attr_range_view(db, loc.krate, *ast_id);
    duplicated(cx, walk, node, name, invoked.syntax().text_range())
}

/// [`duplicate_attribute`] of a `#[test]`, `#[bench]` or `#[test_case]` in a `cfg(test)` crate,
/// which nameres resolves but makes no call of (`collect_attr_macros`): the item `node` stays in
/// `tree`, and the attribute rustc expands is the first of its own that resolves to the macro.
fn unexpanded_test<'db>(
    cx: &Cx<'_, 'db>,
    walk: &Walk<'db>,
    tree: &Tree<'db>,
    node: ast::Node<'db>,
) -> Option<Emitted> {
    let owner = ast::AnyHasAttrs::cast(node)?;
    let (name, invoked) = walk.attrs(owner).find_map(|(meta, _)| {
        let path = meta.path()?;
        if !matches!(path.segment()?.name_ref()?.text(), "test" | "bench" | "test_case") {
            return None;
        }
        let Some(PathResolution::Def(ModuleDef::Macro(mac))) = cx.vsema.resolve_path(path) else { return None };
        let MacroDefKind::BuiltInAttr(_, expander) = MacroId::from(mac).definition(cx.db).kind else { return None };
        let name = match expander {
            BuiltinAttrExpander::Test => "test",
            BuiltinAttrExpander::Bench => "bench",
            BuiltinAttrExpander::TestCase => "test_case",
            _ => return None,
        };
        Some((name, meta.syntax().text_range()))
    })?;
    duplicated(cx, walk, InFile::new(tree.file, node), name, invoked)
}

/// `duplicate_macro_attributes` of the attribute `invoked` of the builtin attribute macro
/// `name` on the item `node`: at the first other attribute of that name.
fn duplicated(
    cx: &Cx<'_, '_>,
    walk: &Walk<'_>,
    node: InFile<ast::Node<'_>>,
    name: &str,
    invoked: TextRange,
) -> Option<Emitted> {
    // rustc does not look for it on an item that is a statement.
    if node.value.parent().is_some_and(|it| matches!(it.kind(), SyntaxKind::STMT_LIST | SyntaxKind::MACRO_STMTS)) {
        return None;
    }
    let owner = ast::AnyHasAttrs::cast(node.value)?;
    let (meta, attr) = walk
        .attrs(owner)
        .find(|(meta, _)| meta.syntax().text_range() != invoked && meta.simple_name() == Some(name))?;
    // An attribute `cfg_attr` gives is spanned by what `cfg_attr` holds of it.
    let range = match meta.syntax().parent().is_some_and(|it| it == attr.syntax()) {
        true => attr.syntax().text_range(),
        false => meta.syntax().text_range(),
    };
    let mut diag = Diag::lint("duplicate_macro_attributes", "duplicated attribute", cx.span(node.with_value(range)));
    let at = enclosing(cx, node);
    source_fact(cx, &mut diag, at);
    Some(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at))
}

/// The node rustc's expansion buffers a lint of the macro call or attribute macro `node` at
/// (`lint_node_id`): the node around the call, not the call itself (it is replaced), and for
/// a call at the top of an expansion, the node around the call that expansion is of.
fn enclosing(cx: &Cx<'_, '_>, node: InFile<ast::Node<'_>>) -> InFile<SyntaxNodePtr> {
    let mut node = node;
    loop {
        let parent = node.value.parent();
        match parent {
            Some(parent) if matches!(parent.kind(), SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_TYPE) => {
                node = node.with_value(parent);
            }
            Some(parent) if parent.parent().is_some() || node.file_id.macro_file().is_none() => {
                return InFile::new(node.file_id, ast::ptr(parent));
            }
            _ => match node.file_id.macro_file() {
                Some(call) => node = call.loc(cx.db).to_node_view(cx.db),
                None => return InFile::new(node.file_id, ast::ptr(node.value)),
            },
        }
    }
}
