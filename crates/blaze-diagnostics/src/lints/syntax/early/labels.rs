//! Labels: `unused_labels`, which late resolution buffers, and the warning it gives when a label
//! shadows one already in scope.
//!
//! `unused_labels` costs one pass over each body's expression column, a buffer of the labels it
//! declares and a bitset over its dense `u32` label indices of those used, both reused across
//! bodies: no hashing, nothing allocated per label (rustc keeps an `FxIndexMap` of every label it
//! declares). The shadowing check runs inside the early pass's one walk (`super::lints`) and
//! walks up from each label without collecting its ancestors; its names are compared in place,
//! and a context is computed only once two names match.
//!
//! Follows rustc's `compiler/rustc_resolve/src/late.rs` (`with_resolved_label`, `resolve_label`,
//! `resolve_block`) and `compiler/rustc_resolve/src/late/diagnostics.rs`
//! (`signal_label_shadowing`) (MIT or Apache-2.0).

use blaze_ids::bits::BitSet;
use hir::{HirFileId, InFile};
use hir_def::{expr_store::Body, hir::Expr, nameres::demand::every_body};
use span::SyntaxContext;
use syntax::{
    SyntaxKind, T, TextRange,
    view::{self as ast, AstNode as _},
};

use super::ctxt;
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, source_fact},
    lower::Phase,
    model::{Diag, Level, MultiSpan},
};

/// `unused_labels`: each label of a loop or block no `break` or `continue` resolved to, but
/// those whose name starts with `'_`. A desugared label (of a `try` block) is not one.
pub(super) fn unused(cx: &Cx<'_, '_>) -> Vec<Emitted> {
    // alloc: once per crate, empty until a label is unused
    let mut out = Vec::new();
    // alloc: once per crate, reused across bodies
    let (mut declared, mut used) = (Vec::new(), BitSet::<u32>::default());
    for owner in every_body(cx.db, cx.krate.base()) {
        let (body, source_map) = Body::with_source_map(cx.db, owner);
        declared.clear();
        used.reset(0);
        for (_, expr) in body.exprs() {
            match expr {
                Expr::Loop { label: Some(label), .. } | Expr::Block { label: Some(label), .. } => declared.push(label),
                Expr::Break { label: Some(label), .. } | Expr::Continue { label: Some(label) } => {
                    used.insert_growing(u32::from(label.into_raw()));
                }
                _ => {}
            }
        }
        for &label in &declared {
            if used.contains(u32::from(label.into_raw())) {
                continue;
            }
            let source = source_map.label_syntax(label);
            let ptr = source.value.syntax_node_ptr();
            if ptr.kind() != SyntaxKind::LABEL {
                continue;
            }
            let node = cx.syntax_view(InFile::new(source.file_id, ptr));
            let Some(lifetime) = node.value.children().find_map(ast::Lifetime::cast) else { continue };
            if lifetime.text().as_bytes().get(1) == Some(&b'_') {
                continue;
            }
            let Some(labeled) = node.value.parent() else { continue };
            let at = InFile::new(node.file_id, ast::ptr(labeled));
            let span = cx.span(InFile::new(node.file_id, lifetime.syntax().text_range()));
            let mut diag = Diag::lint("unused_labels", "unused label", span);
            source_fact(cx, &mut diag, at);
            out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
        }
    }
    out
}

/// The warning resolution gives when the label `node` (of `tree`) resolves, as a `break` to it
/// would, to a label of an enclosing loop or block. Called in walk order, so in the order rustc's
/// visitor declares them.
pub(super) fn shadowing(cx: &Cx<'_, '_>, tree: &Tree<'_>, node: ast::Node<'_>, out: &mut Vec<Emitted>) {
    let Some(lifetime) = node.children().find_map(ast::Lifetime::cast) else { return };
    let Some(labeled) = node.parent() else { return };
    let range = lifetime.syntax().text_range();
    let at = InFile::new(tree.file, ast::ptr(labeled));
    let hygiene = ctxt(cx, tree.file, range);
    let Some(orig) = resolve(cx, InFile::new(tree.file, labeled), lifetime.text(), hygiene) else { return };
    let name = lifetime.text();
    let shadower = cx.span(InFile::new(tree.file, range));
    let mut diag =
        // alloc: the diagnostic's text and span, as is the rest of `shadowing`
        Diag::new(Level::Warning, None, format!("label name `{name}` shadows a label name that is already in scope"));
    // clone: as above
    diag.span = MultiSpan::of(shadower.clone());
    diag.span_label(cx.span(orig), "first declared here");
    // alloc: as above
    diag.span_label(shadower, format!("label `{name}` already in scope"));
    source_fact(cx, &mut diag, at);
    out.push(Emitted::unplaced(diag, Phase::Resolve, Pass::Buffered));
}

/// rustc's `resolve_label` from the label ribs enclosing `labeled`: the range of the lifetime of
/// the label `name` resolves to, when it is reachable (no closure, coroutine or constant lies
/// between). Walks up through each expansion to its call; the root of an expansion and its call
/// are no rib.
fn resolve(
    cx: &Cx<'_, '_>,
    labeled: InFile<ast::Node<'_>>,
    name: &str,
    mut hygiene: SyntaxContext,
) -> Option<InFile<TextRange>> {
    let (mut file, mut child) = (labeled.file_id, labeled.value);
    let mut key: Option<SyntaxContext> = None;
    loop {
        let Some(parent) = child.parent() else {
            let call = file.macro_file()?.loc(cx.db).to_node_view(cx.db);
            (file, child) = (call.file_id, call.value);
            continue;
        };
        if barrier(parent, child) {
            return None;
        }
        if parent.kind() == SyntaxKind::STMT_LIST && macro_definitions(cx, file, parent, child, &mut hygiene) {
            key = None;
        }
        if let Some(label) = label_over(parent, child)
            && let Some(lifetime) = label.lifetime()
            && lifetime.text() == name
        {
            let range = lifetime.syntax().text_range();
            let key = *key.get_or_insert_with(|| hygiene.normalize_to_macro_rules(cx.db));
            if ctxt(cx, file, range).normalize_to_macro_rules(cx.db) == key {
                return Some(InFile::new(file, range));
            }
        }
        child = parent;
    }
}

/// The label `parent` declares over `child`: rustc's label rib of a loop or block covers a
/// `loop`'s body, a `while`'s condition and body, a `for`'s pattern and body (not what it
/// iterates) and a block's statements.
fn label_over<'t>(parent: ast::Node<'t>, child: ast::Node<'t>) -> Option<ast::Label<'t>> {
    let label = || parent.children().find_map(ast::Label::cast);
    let mut blocks = parent.children().filter(|it| it.kind() == SyntaxKind::BLOCK_EXPR);
    match parent.kind() {
        // `LoopExpr::loop_body`: its first block.
        SyntaxKind::LOOP_EXPR => blocks.next().filter(|&body| body == child).and_then(|_| label()),
        SyntaxKind::WHILE_EXPR if child.kind() != SyntaxKind::LABEL => label(),
        SyntaxKind::FOR_EXPR => {
            // `ForExpr::loop_body`: its second block, else its first.
            let (first, second) = (blocks.next(), blocks.next());
            let in_scope = second.or(first).is_some_and(|body| body == child)
                || parent.children().find(|it| ast::Pat::can_cast(it.kind())).is_some_and(|pat| pat == child);
            in_scope.then(label).flatten()
        }
        SyntaxKind::BLOCK_EXPR if child.kind() == SyntaxKind::STMT_LIST => label(),
        _ => None,
    }
}

/// Whether rustc's label ribs have a barrier at `parent` over `child`: a closure, a coroutine
/// (`async` or `gen` block), a constant (an item, an inline `const` block, an array's length, a
/// const argument or parameter default, a discriminant) or an item's own scope.
fn barrier(parent: ast::Node<'_>, child: ast::Node<'_>) -> bool {
    match parent.kind() {
        SyntaxKind::CLOSURE_EXPR | SyntaxKind::CONST_ARG | SyntaxKind::CONST_PARAM | SyntaxKind::VARIANT => {
            true
        }
        SyntaxKind::BLOCK_EXPR => parent
            .children_with_tokens()
            .any(|it| matches!(it.kind(), T![async] | T![gen] | T![const])),
        SyntaxKind::ARRAY_EXPR => parent
            .children_with_tokens()
            .find(|it| it.kind() == T![;])
            .is_some_and(|semi| child.text_range().start() > semi.text_range().start()),
        SyntaxKind::ARRAY_TYPE => ast::Expr::can_cast(child.kind()) || child.kind() == SyntaxKind::CONST_ARG,
        SyntaxKind::MACRO_CALL => false,
        kind => ast::Item::can_cast(kind) || kind == SyntaxKind::SOURCE_FILE,
    }
}

/// rustc's `MacroDefinition` ribs of a block (`list`, in `file`): each macro defined by a
/// statement before `child`, the last first. While the outermost mark of the label's context
/// is of a call of one of them, the label is taken as written where that macro is defined
/// (`remove_mark`), and only the macros before it are left. Whether a mark was removed. A label
/// no macro wrote costs one lookup of its context's outermost mark.
fn macro_definitions(
    cx: &Cx<'_, '_>,
    file: HirFileId,
    list: ast::Node<'_>,
    child: ast::Node<'_>,
    hygiene: &mut SyntaxContext,
) -> bool {
    let mut bound = child.text_range().start();
    let mut removed = false;
    while let Some(call) = hygiene.outer_expn(cx.db) {
        let Some(ast_id) = hir_expand::MacroCallId::from(call).loc(cx.db).def.ast_id().left() else { break };
        if ast_id.file_id != file {
            break;
        }
        let ptr = ast_id.to_ptr(cx.db).syntax_node_ptr();
        let start = ptr.text_range().start();
        let defined_here = start < bound
            && list.children().any(|stmt| {
                matches!(stmt.kind(), SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF) && ast::ptr(stmt) == ptr
            });
        if !defined_here {
            break;
        }
        hygiene.remove_mark(cx.db);
        removed = true;
        bound = start;
    }
    removed
}
