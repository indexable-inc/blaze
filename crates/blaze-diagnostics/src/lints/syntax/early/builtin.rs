//! The lints of rustc's early lint passes in `builtin.rs` and beside it: `while_true`,
//! `unsafe_code`, `special_module_name`, `anonymous_parameters`,
//! `ellipsis_inclusive_range_patterns` (an error, E0783, from the 2021 edition on),
//! `double_negations`, `ambiguous_negative_literals` and `redundant_semicolons`.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`WhileTrue`, `UnsafeCode`,
//! `SpecialModuleName`, `AnonymousParameters`, `EllipsisInclusiveRangePatterns`,
//! `DoubleNegations`), `compiler/rustc_lint/src/precedence.rs`,
//! `compiler/rustc_lint/src/redundant_semicolon.rs`, their diagnostics in
//! `compiler/rustc_lint/src/lints.rs` and `compiler/rustc_lint/src/diagnostics.rs`, and
//! `compiler/rustc_parse/src/parser/stmt.rs` (`parse_full_stmt`: which `;` is a statement of
//! its own) (MIT or Apache-2.0).

use hir::{HirFileId, InFile};
use hir_expand::{MacroDefKind, builtin::BuiltinFnLikeExpander};
use span::Edition;
use syntax::{
    NodeOrToken, SyntaxKind, TextRange, T,
    view::{self as ast, AstNode as _, HasLoopBody as _, HasName as _, LiteralKind, RangeItem as _, UnaryOp},
};

use super::{Expansions, allows_unsafe, ctxt, range_from_expansion};
use crate::{
    cx::{Cx, item_range_view},
    lints::{
        registry::LintId,
        syntax::{Emitted, Pass, Tree, Walk, from_expansion, source_fact},
    },
    lower::Phase,
    model::{Applicability, Diag, SuggestionStyle},
};

/// The checks of this part, as the early pass's one walk (`super::lints`) calls them: each node
/// once, dispatched on its kind to the checks that look at that kind, the shadowing of labels
/// (`labels::shadowing`) included. Nothing is allocated per node: the statement buffer is reused
/// across blocks, and a diagnostic's strings are built only once it is given.
pub(super) struct Builtin<'a, 'w, 'db> {
    expansions: &'a Expansions<'w, 'db>,
    anonymous: bool,
    /// Whether `unsafe_code` and `ambiguous_negative_literals`, allowed by default, may fire:
    /// the nodes they look at are not read otherwise.
    unsafe_code: bool,
    negative_literals: bool,
    stmts: Vec<Stmt>,
    /// The range pattern of the tree whose `...` was linted with the `&` before it: nothing in
    /// it is linted again (`EllipsisInclusiveRangePatterns::node_id`).
    linted_ref: Option<TextRange>,
}

impl<'a, 'w, 'db> Builtin<'a, 'w, 'db> {
    pub(super) fn new(cx: &Cx<'_, '_>, expansions: &'a Expansions<'w, 'db>) -> Builtin<'a, 'w, 'db> {
        Builtin {
            expansions,
            anonymous: cx.edition == Edition::Edition2015,
            unsafe_code: cx.lint_may_fire(LintId::named("unsafe_code")),
            negative_literals: cx.lint_may_fire(LintId::named("ambiguous_negative_literals")),
            // alloc: empty at setup; one buffer reused across blocks
            stmts: Vec::new(),
            linted_ref: None,
        }
    }

    /// What is checked once for the crate: `special_module_name`.
    pub(super) fn krate(&self, cx: &Cx<'_, '_>, walk: &Walk<'_>, out: &mut Vec<Emitted>) {
        special_module_names(cx, walk, self.expansions, out);
    }

    /// What is checked once for a tree, before its nodes: `global_asm!`.
    pub(super) fn tree(&mut self, cx: &Cx<'_, '_>, tree: &Tree<'_>, out: &mut Vec<Emitted>) {
        self.linted_ref = None;
        if self.unsafe_code {
            global_asm(cx, tree, out);
        }
    }

    /// The checks of `node`, of kind `kind`.
    pub(super) fn node(
        &mut self,
        cx: &Cx<'_, '_>,
        walk: &Walk<'_>,
        tree: &Tree<'db>,
        node: ast::Node<'db>,
        kind: SyntaxKind,
        out: &mut Vec<Emitted>,
    ) {
        match kind {
            SyntaxKind::BLOCK_EXPR
            | SyntaxKind::TRAIT
            | SyntaxKind::IMPL
            | SyntaxKind::EXTERN_BLOCK
            | SyntaxKind::MACRO_RULES
            | SyntaxKind::MACRO_DEF => {
                if self.unsafe_code {
                    unsafe_code(cx, walk, tree, node, out);
                }
            }
            SyntaxKind::FN => {
                if self.unsafe_code {
                    unsafe_code(cx, walk, tree, node, out);
                }
                if self.anonymous {
                    anonymous_parameters(cx, tree, node, out);
                }
            }
            SyntaxKind::REF_PAT | SyntaxKind::RANGE_PAT => {
                if self.linted_ref.is_some_and(|range| !range.contains_range(node.text_range())) {
                    self.linted_ref = None;
                }
                if self.linted_ref.is_none() {
                    self.linted_ref = ellipsis_range(cx, tree, node, out);
                }
            }
            SyntaxKind::WHILE_EXPR => while_true(cx, tree, node, out),
            SyntaxKind::STMT_LIST => redundant_semicolons(cx, self.expansions, tree, node, &mut self.stmts, out),
            SyntaxKind::PREFIX_EXPR => {
                if self.negative_literals {
                    ambiguous_negative_literal(cx, tree, node, out);
                }
                double_negation(cx, tree, node, out);
            }
            SyntaxKind::LABEL => super::labels::shadowing(cx, tree, node, out),
            _ => {}
        }
    }
}

/// A lint of the early lint pass at `at`, with the fact of the source it read.
fn emitted(cx: &Cx<'_, '_>, mut diag: Diag, pass: Pass, at: InFile<syntax::SyntaxNodePtr>) -> Emitted {
    source_fact(cx, &mut diag, at);
    Emitted::new(diag, Phase::Resolve, pass, at)
}

/// rustc's `span` of a node: from its first token after its attributes.
fn range_of(node: ast::Node<'_>) -> TextRange {
    item_range_view(node)
}

/// `while true { .. }`: `WhileTrue`.
fn while_true(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let Some(it) = ast::WhileExpr::cast(node) else { return };
    let Some(cond) = it.condition() else { return };
    let mut peeled = cond;
    while let ast::Expr::ParenExpr(paren) = peeled {
        let Some(inner) = paren.expr() else { return };
        peeled = inner;
    }
    let ast::Expr::Literal(lit) = peeled else { return };
    if lit.kind() != LiteralKind::Bool(true) || from_expansion(cx, InFile::new(tree.file, cond.syntax())) {
        return;
    }
    let range = TextRange::new(range_of(node).start(), cond.syntax().text_range().end());
    let span = cx.span(InFile::new(tree.file, range));
    let replace = match it.label().and_then(|it| it.lifetime()) {
        // alloc: the lint's text, for a lint found
        Some(label) => format!("{}: loop", label.text()),
        // alloc: the lint's text, for a lint found
        None => "loop".to_owned(),
    };
    // clone: a lint's span, for a lint found
    let mut diag = Diag::lint("while_true", "denote infinite loops with `loop { ... }`", span.clone());
    diag.suggestion_with_style(
        // alloc: the lint's text, for a lint found
        vec![(span, replace)],
        "use `loop`",
        Applicability::MachineApplicable,
        SuggestionStyle::HideCodeInline,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// `unsafe` blocks, traits, impls, `extern` blocks and functions, and
/// `#[allow_internal_unsafe]` on a macro: `UnsafeCode`. Called only for the kinds it checks, one
/// cast each.
fn unsafe_code(cx: &Cx<'_, '_>, walk: &Walk<'_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let unsafe_token = |kind: SyntaxKind| node.children_with_tokens().any(|it| it.kind() == kind);
    let (range, message) = match node.kind() {
        SyntaxKind::BLOCK_EXPR if unsafe_token(T![unsafe]) => (range_of(node), "usage of an `unsafe` block"),
        SyntaxKind::TRAIT if unsafe_token(T![unsafe]) => (range_of(node), "declaration of an `unsafe` trait"),
        SyntaxKind::IMPL
            if unsafe_token(T![unsafe]) && ast::Impl::cast(node).is_some_and(|it| it.trait_().is_some()) =>
        {
            (range_of(node), "implementation of an `unsafe` trait")
        }
        SyntaxKind::EXTERN_BLOCK if unsafe_token(T![unsafe]) => (range_of(node), "usage of an `unsafe extern` block"),
        SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF => {
            let Some(range) = allow_internal_unsafe(walk, node) else { return };
            (
                range,
                "`allow_internal_unsafe` allows defining macros using unsafe without triggering the `unsafe_code` lint at their call site",
            )
        }
        SyntaxKind::FN if unsafe_token(T![unsafe]) => {
            let message = match node.parent().map(|it| it.kind()) {
                Some(SyntaxKind::EXTERN_ITEM_LIST) => return,
                Some(SyntaxKind::ASSOC_ITEM_LIST) if !node.children().any(|it| it.kind() == SyntaxKind::BLOCK_EXPR) => {
                    "declaration of an `unsafe` method"
                }
                Some(SyntaxKind::ASSOC_ITEM_LIST) => "implementation of an `unsafe` method",
                _ => "declaration of an `unsafe` function",
            };
            (range_of(node), message)
        }
        _ => return,
    };
    if allows_unsafe(cx, tree.file, range) {
        return;
    }
    let diag = Diag::lint("unsafe_code", message, cx.span(InFile::new(tree.file, range)));
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// The range rustc spans `#[allow_internal_unsafe]` of a macro definition by: the attribute, or
/// what `cfg_attr` holds of it. A definition without attributes costs one scan of its children.
fn allow_internal_unsafe(walk: &Walk<'_>, node: ast::Node<'_>) -> Option<TextRange> {
    if !node.children().any(|it| it.kind() == SyntaxKind::ATTR) {
        return None;
    }
    let owner = ast::AnyHasAttrs::cast(node)?;
    walk.attrs(owner).find_map(|(meta, attr)| {
        let named = meta.path().filter(|it| it.qualifier().is_none()).and_then(|it| it.segment()).and_then(|it| {
            it.syntax().first_token()
        });
        if named.is_none_or(|it| it.text() != "allow_internal_unsafe") {
            return None;
        }
        let written = attr.meta().is_some_and(|it| it.syntax() == meta.syntax());
        Some(if written { attr.syntax().text_range() } else { meta.syntax().text_range() })
    })
}

/// A call of `global_asm!`, the item it expands to: `UnsafeCode`'s `ItemKind::GlobalAsm`.
fn global_asm(cx: &Cx<'_, '_>, tree: &Tree<'_>, out: &mut Vec<Emitted>) {
    let Some(call) = tree.call else { return };
    let loc = call.loc(cx.db);
    if !matches!(loc.def.kind, MacroDefKind::BuiltIn(_, BuiltinFnLikeExpander::GlobalAsm)) {
        return;
    }
    let node = loc.to_node_view(cx.db);
    let Some(macro_call) = ast::MacroCall::cast(node.value) else { return };
    let start = range_of(node.value).start();
    let end = macro_call.token_tree().map_or(node.value.text_range().end(), |it| it.syntax().text_range().end());
    let range = TextRange::new(start, end);
    if allows_unsafe(cx, node.file_id, range) {
        return;
    }
    let mut diag = Diag::lint("unsafe_code", "usage of `core::arch::global_asm`", cx.span(node.with_value(range)));
    diag.note("using this macro is unsafe even though it does not need an `unsafe` block");
    out.push(emitted(cx, diag, Pass::Early, node.map(ast::ptr)));
}

/// `mod lib;` and `mod main;` among the items of the crate root: `SpecialModuleName`, which
/// rustc checks in `check_crate`.
fn special_module_names(cx: &Cx<'_, '_>, walk: &Walk<'_>, expansions: &Expansions<'_, '_>, out: &mut Vec<Emitted>) {
    let root = cx.krate.root_module(cx.db);
    let Some(file) = root.as_source_file_id(cx.db) else { return };
    let Some(tree) = walk.tree(HirFileId::from(file)) else { return };
    crate_items(tree, tree.root, expansions, &mut |in_tree, item| {
        let Some(module) = ast::Module::cast(item) else { return };
        if module.item_list().is_some() {
            return;
        }
        let Some(name) = module.name() else { return };
        let name = name.text();
        let name = name.strip_prefix("r#").unwrap_or(name);
        if name != "lib" && name != "main" {
            return;
        }
        let Some(owner) = ast::AnyHasAttrs::cast(item) else { return };
        if walk.attrs(owner).any(|(meta, _)| meta.simple_name() == Some("path")) {
            return;
        }
        let span = cx.span(InFile::new(in_tree.file, range_of(item)));
        let mut diag = match name {
            "lib" => {
                let mut diag = Diag::lint("special_module_name", "found module declaration for lib.rs", span);
                diag.note("lib.rs is the root of this crate's library target");
                diag.help("to refer to it from other targets, use the library's name as the path");
                diag
            }
            "main" => {
                let mut diag = Diag::lint("special_module_name", "found module declaration for main.rs", span);
                diag.note("a binary crate cannot be used as library");
                diag
            }
            _ => return,
        };
        let at = tree.ptr(tree.root);
        source_fact(cx, &mut diag, InFile::new(in_tree.file, ast::ptr(item)));
        out.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
    });
}

/// The items of the crate root as rustc's AST has them once expanded: those of the file, and
/// in place of a macro call or of an item an attribute macro replaces, what it expands to.
fn crate_items<'w, 'db, F: FnMut(&'w Tree<'db>, ast::Node<'db>)>(
    tree: &'w Tree<'db>,
    parent: ast::Node<'db>,
    expansions: &Expansions<'w, 'db>,
    visit: &mut F,
) {
    for child in parent.children() {
        if let Some(expanded) = expansions.of_call(tree.file, child)
            && (ast::MacroCall::can_cast(child.kind()) || tree.replaced.contains(&child))
        {
            crate_items(expanded, expanded.root, expansions, visit);
            continue;
        }
        if tree.skipped(child.text_range()) || !ast::Item::can_cast(child.kind()) {
            continue;
        }
        visit(tree, child);
    }
}

/// A trait method's parameter with a type and no name, in the 2015 edition:
/// `AnonymousParameters`.
fn anonymous_parameters(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let Some(it) = ast::Fn::cast(node) else { return };
    let in_trait = node
        .parent()
        .filter(|it| it.kind() == SyntaxKind::ASSOC_ITEM_LIST)
        .and_then(|it| it.parent())
        .is_some_and(|it| it.kind() == SyntaxKind::TRAIT);
    if !in_trait {
        return;
    }
    let Some(params) = it.param_list() else { return };
    for param in params.params() {
        if param.pat().is_some() || tree.skipped(param.syntax().text_range()) {
            continue;
        }
        let Some(ty) = param.ty() else { continue };
        let span = cx.span(InFile::new(tree.file, ty.syntax().text_range()));
        let (ty_snip, applicability) = match cx.snippet(&span) {
            // alloc: the lint's text, for a lint found
            Some(snip) => (snip.to_owned(), Applicability::MachineApplicable),
            // alloc: the lint's text, for a lint found
            None => ("<type>".to_owned(), Applicability::HasPlaceholders),
        };
        let mut diag = Diag::lint(
            "anonymous_parameters",
            "anonymous parameters are deprecated and will be removed in the next edition",
            // clone: a lint's span, for a lint found
            span.clone(),
        );
        // alloc: the lint's text, for a lint found
        diag.suggestion(span, "try naming the parameter or explicitly ignoring it", format!("_: {ty_snip}"), applicability);
        out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
    }
}

/// A `...` range pattern: `EllipsisInclusiveRangePatterns`. The lint before the 2021 edition
/// of the `...`, error E0783 from it on. The range of the pattern linted with the `&` before
/// it, when it is, whose patterns are not linted again.
fn ellipsis_range(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) -> Option<TextRange> {
    fn ellipsis(pat: ast::Pat<'_>) -> Option<(ast::RangePat<'_>, ast::Token<'_>, ast::Pat<'_>)> {
        let ast::Pat::RangePat(range) = pat else { return None };
        let op = range.op_token().filter(|it| it.kind() == T![...])?;
        let end = range.end()?;
        Some((range, op, end))
    }
    let pat = ast::Pat::cast(node)?;
    let (parenthesise, (range, join, end)) = match pat {
        ast::Pat::RefPat(it) => (true, ellipsis(it.pat()?)?),
        _ => (false, ellipsis(pat)?),
    };
    let edition = ctxt(cx, tree.file, join.text_range()).edition(cx.db);
    let pat_span = cx.span(InFile::new(tree.file, node.text_range()));
    let join_span = cx.span(InFile::new(tree.file, join.text_range()));
    let message = "`...` range patterns are deprecated";
    let suggestion = "use `..=` for an inclusive range";
    let at = tree.ptr(node);
    let mut diag = if parenthesise {
        let end = end.syntax().text();
        let replace = match range.start() {
            // alloc: the lint's text, for a lint found
            Some(start) => format!("&({}..={end})", start.syntax().text()),
            // alloc: the lint's text, for a lint found
            None => format!("&(..={end})"),
        };
        if edition >= Edition::Edition2021 {
            // clone: a lint's span, for a lint found
            let mut diag = Diag::error(Some("E0783"), message, pat_span.clone());
            // alloc: the lint's text, for a lint found
            diag.suggestion_with_style(vec![(pat_span, replace)], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
            diag
        } else {
            // clone: a lint's span, for a lint found
            let mut diag = Diag::lint("ellipsis_inclusive_range_patterns", message, pat_span.clone());
            diag.suggestion(pat_span, suggestion, replace, Applicability::MachineApplicable);
            diag
        }
    } else if edition >= Edition::Edition2021 {
        let mut diag = Diag::error(Some("E0783"), message, pat_span);
        // alloc: the lint's text, for a lint found
        diag.suggestion_with_style(vec![(join_span, "..=".to_owned())], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
        diag
    } else {
        // clone: a lint's span, for a lint found
        let mut diag = Diag::lint("ellipsis_inclusive_range_patterns", message, join_span.clone());
        // alloc: the lint's text, for a lint found
        diag.suggestion_with_style(vec![(join_span, "..=".to_owned())], suggestion, Applicability::MachineApplicable, SuggestionStyle::HideCodeInline);
        diag
    };
    source_fact(cx, &mut diag, at);
    out.push(match diag.is_lint() {
        true => Emitted::new(diag, Phase::Resolve, Pass::Early, at),
        false => Emitted::placed(diag, Phase::Resolve, Pass::Early, at),
    });
    parenthesise.then(|| node.text_range())
}

/// `--x`, linted at the innermost `--` of a chain of `-`: `DoubleNegations`.
fn double_negation(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let neg = |it: &ast::PrefixExpr<'_>| it.op_kind() == Some(UnaryOp::Neg);
    let Some(outer) = ast::PrefixExpr::cast(node).filter(neg) else { return };
    let Some(ast::Expr::PrefixExpr(inner)) = outer.expr() else { return };
    if !neg(&inner) {
        return;
    }
    if let Some(ast::Expr::PrefixExpr(it)) = inner.expr()
        && neg(&it)
    {
        return;
    }
    let (outer_range, inner_range) = (range_of(node), range_of(inner.syntax()));
    if ctxt(cx, tree.file, outer_range) != ctxt(cx, tree.file, inner_range) {
        return;
    }
    let at = |range: TextRange| cx.span(InFile::new(tree.file, range));
    let mut diag = Diag::lint("double_negations", "use of a double negation", at(outer_range));
    diag.note("the prefix `--` could be misinterpreted as a decrement operator which exists in other languages");
    diag.note("use `-= 1` if you meant to decrement the value");
    diag.suggestion_with_style(
        // alloc: the lint's text, for a lint found
        vec![
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(inner_range.start())), "(".to_owned()),
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(inner_range.end())), ")".to_owned()),
        ],
        "add parentheses for clarity",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// `-1.abs()`: a `-` before method calls on a number literal: `Precedence`.
fn ambiguous_negative_literal(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let Some(expr) = ast::PrefixExpr::cast(node) else { return };
    if expr.op_kind() != Some(UnaryOp::Neg) {
        return;
    }
    let Some(operand) = expr.expr() else { return };
    let mut arg = operand;
    let mut at_least_one = false;
    while let ast::Expr::MethodCallExpr(call) = arg {
        at_least_one = true;
        let Some(receiver) = call.receiver() else { return };
        arg = receiver;
    }
    let ast::Expr::Literal(lit) = arg else { return };
    if !at_least_one || !matches!(lit.kind(), LiteralKind::IntNumber(_) | LiteralKind::FloatNumber(_)) {
        return;
    }
    let at = |range: TextRange| cx.span(InFile::new(tree.file, range));
    let (expr_range, arg_range, operand_range) = (range_of(node), range_of(arg.syntax()), range_of(operand.syntax()));
    let mut diag = Diag::lint(
        "ambiguous_negative_literals",
        "`-` has lower precedence than method calls, which might be unexpected",
        at(expr_range),
    );
    diag.note("e.g. `-4.abs()` equals `-4`; while `(-4).abs()` equals `4`");
    diag.suggestion_with_style(
        // alloc: the lint's text, for a lint found
        vec![
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(expr_range.start())), "(".to_owned()),
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(arg_range.end())), ")".to_owned()),
        ],
        "add parentheses around the `-` and the literal to call the method on a negative literal",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    diag.suggestion_with_style(
        // alloc: the lint's text, for a lint found
        vec![
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(operand_range.start())), "(".to_owned()),
            // alloc: the lint's text, for a lint found
            (at(TextRange::empty(operand_range.end())), ")".to_owned()),
        ],
        "add parentheses around the literal and the method call to keep the current behavior",
        Applicability::MaybeIncorrect,
        SuggestionStyle::ShowCode,
    );
    out.push(emitted(cx, diag, Pass::Early, tree.ptr(node)));
}

/// A statement of a block as rustc's AST has it once expanded: an empty one (a `;` of its
/// own) where it is written, or any other. One per statement, in a buffer reused across blocks.
#[derive(Clone, Copy)]
enum Stmt {
    Empty(InFile<TextRange>),
    Other,
}

/// `;` after a statement that already ends: `RedundantSemicolons`, of each block.
fn redundant_semicolons<'db>(
    cx: &Cx<'_, '_>,
    expansions: &Expansions<'_, 'db>,
    tree: &Tree<'db>,
    node: ast::Node<'db>,
    stmts: &mut Vec<Stmt>,
    out: &mut Vec<Emitted>,
) {
    let Some(block) = node.parent() else { return };
    stmts.clear();
    statements(tree, node, expansions, stmts);
    let mut seq: Option<(InFile<TextRange>, bool)> = None;
    let mut flush = |seq: &mut Option<(InFile<TextRange>, bool)>| {
        let Some((range, multiple)) = seq.take() else { return };
        let span = cx.span(range);
        let message = match multiple {
            true => "unnecessary trailing semicolons",
            false => "unnecessary trailing semicolon",
        };
        // clone: a lint's span, for a lint found
        let mut diag = Diag::lint("redundant_semicolons", message, span.clone());
        if !range_from_expansion(cx, range) {
            let remove = match multiple {
                true => "remove these semicolons",
                false => "remove this semicolon",
            };
            diag.suggestion(span, remove, "", Applicability::MaybeIncorrect);
        }
        out.push(emitted(cx, diag, Pass::Early, tree.ptr(block)));
    };
    for &stmt in stmts.iter() {
        let Stmt::Empty(range) = stmt else {
            flush(&mut seq);
            continue;
        };
        if let Some((at, multiple)) = &mut seq
            && at.file_id == range.file_id
        {
            *at = at.with_value(at.value.cover(range.value));
            *multiple = true;
        } else {
            flush(&mut seq);
            seq = Some((range, false));
        }
    }
    flush(&mut seq);
}

/// The statements of `parent` (a block's statements, or what a macro call in statement
/// position expands to), in order, without what `cfg` removes, each macro call in statement
/// position replaced by its expansion's statements.
fn statements<'w, 'db>(tree: &'w Tree<'db>, parent: ast::Node<'db>, expansions: &Expansions<'w, 'db>, out: &mut Vec<Stmt>) {
    for child in parent.children_with_tokens() {
        let child = match child {
            NodeOrToken::Token(token) => {
                if token.kind() == T![;] {
                    out.push(Stmt::Empty(InFile::new(tree.file, token.text_range())));
                }
                continue;
            }
            NodeOrToken::Node(node) => node,
        };
        if matches!(child.kind(), SyntaxKind::ATTR | SyntaxKind::DOC_COMMENT) || tree.skipped(child.text_range()) {
            continue;
        }
        let macro_call = |expr: ast::Node<'db>| expr.children().find(|it| it.kind() == SyntaxKind::MACRO_CALL);
        let call = match child.kind() {
            SyntaxKind::EXPR_STMT => child
                .children()
                .find(|it| it.kind() != SyntaxKind::ATTR)
                .filter(|it| it.kind() == SyntaxKind::MACRO_EXPR)
                .and_then(macro_call),
            SyntaxKind::MACRO_EXPR => macro_call(child),
            SyntaxKind::MACRO_CALL => Some(child),
            _ => None,
        };
        if let Some(call) = call
            && let Some(expanded) = expansions.of_call(tree.file, call)
            && expanded.root.kind() == SyntaxKind::MACRO_STMTS
        {
            statements(expanded, expanded.root, expansions, out);
            continue;
        }
        out.push(Stmt::Other);
    }
}
