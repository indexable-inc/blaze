//! `unused_doc_comments`: a doc comment or `#[doc]` on what rustdoc never documents. The early
//! lint pass gives it for statements, match arms, expressions, the fields of struct patterns
//! and expressions, generic parameters and `extern` blocks; expansion gives it for macro
//! calls, buffered at the node around the call.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`UnusedDocComment`, `warn_if_doc`),
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinUnusedDocComment`), `compiler/rustc_expand/src/
//! expand.rs` (`InvocationCollector::check_attributes`, `assign_id!`: the node a macro call's
//! lints are buffered at) and `compiler/rustc_expand/src/diagnostics.rs`
//! (`MacroCallUnusedDocComment`) (MIT or Apache-2.0).

use blaze_hash::HashMap;
use hir::{HirFileId, InFile};
use syntax::{
    SyntaxKind, SyntaxNodePtr, TextRange,
    view::{self as ast, AstNode, CommentShape, HasLoopBody as _, HasName as _},
};

use super::super::super::{Emitted, Pass, source_fact};
use super::super::{Attr, AttrKind, Owner, Target};
use crate::{
    cx::{Cx, item_range_view},
    lower::Phase,
    model::Diag,
};

const LINT: &str = "unused_doc_comments";

/// rustc's `UnusedDocComment`: the lints of `owner`'s doc comments, when it is what the early
/// lint pass checks them on.
pub(super) fn unused(cx: &Cx<'_, '_>, owner: &Owner<'_>, emitted: &mut Vec<Emitted>) {
    // Each doc string of an owner below is linted: one without any is left at once.
    if !owner.attrs.iter().any(|it| it.is_doc_comment() || super::is_doc(it)) {
        return;
    }
    let node = owner.node.value;
    let (kind, label, at) = match owner.target {
        Target::Statement => ("statements", item_range_view(node), owner.at()),
        Target::Arm => {
            let Some(arm) = ast::MatchArm::cast(node) else { return };
            let (Some(pat), Some(body)) = (arm.pat(), arm.expr()) else { return };
            let range = TextRange::new(pat.syntax().text_range().start(), body.syntax().text_range().end());
            ("match arms", range, owner.at())
        }
        Target::PatField => {
            let Some(at) = grandparent(owner, SyntaxKind::RECORD_PAT) else { return };
            ("pattern fields", item_range_view(node), at)
        }
        Target::ExprField => {
            let Some(at) = grandparent(owner, SyntaxKind::RECORD_EXPR) else { return };
            ("expression fields", item_range_view(node), at)
        }
        Target::GenericParam { .. } => {
            let name = match node.kind() {
                SyntaxKind::LIFETIME_PARAM => {
                    ast::LifetimeParam::cast(node).and_then(|it| it.lifetime()).map(|it| it.syntax().text_range())
                }
                SyntaxKind::TYPE_PARAM => ast::TypeParam::cast(node).and_then(|it| it.name()).map(|it| it.syntax().text_range()),
                _ => ast::ConstParam::cast(node).and_then(|it| it.name()).map(|it| it.syntax().text_range()),
            };
            let Some(name) = name else { return };
            ("generic parameters", name, owner.at())
        }
        Target::ForeignMod => ("extern blocks", item_range_view(node), owner.at()),
        _ if ast::Expr::can_cast(node.kind()) && !is_block(node) => ("expressions", item_range_view(node), owner.at()),
        _ => return,
    };
    let label = cx.span(InFile::new(owner.node.file_id, label));
    let attrs = &owner.attrs;
    let mut sugared: Option<InFile<TextRange>> = None;
    for (i, attr) in attrs.iter().enumerate() {
        let is_doc_attribute = super::is_doc(attr);
        let is_doc = attr.is_doc_comment() || is_doc_attribute;
        let range = attr.in_file(attr.range());
        if is_doc {
            sugared = Some(match sugared {
                Some(it) if it.file_id == range.file_id => it.map(|it| TextRange::new(it.start(), range.value.end())),
                _ => range,
            });
        }
        if !is_doc_attribute && attrs.get(i + 1).is_some_and(Attr::is_doc_comment) {
            continue;
        }
        let span = sugared.take().unwrap_or(range);
        if !is_doc {
            continue;
        }
        let help = match &attr.kind {
            AttrKind::Doc(doc) if matches!(doc.token().shape(), CommentShape::Block) => "use `/* */` for a plain comment",
            _ => "use `//` for a plain comment",
        };
        let mut diag = Diag::lint(LINT, "unused doc comment", cx.span(span));
        // alloc: the diagnostic's text and span
        diag.span_label(label.clone(), format!("rustdoc does not generate documentation for {kind}"));
        diag.help(help);
        // clone: a pointer, copied for a lint emitted
        source_fact(cx, &mut diag, at.clone());
        // clone: as above
        emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at.clone()));
    }
}

/// rustc's `MacroCallUnusedDocComment`: the doc comment `attr` of a macro call, `span` from the
/// call's first attribute to it.
pub(super) fn macro_call(cx: &Cx<'_, '_>, span: InFile<TextRange>, attr: InFile<TextRange>) -> Diag {
    let mut diag = Diag::lint(LINT, "unused doc comment", cx.span(span));
    diag.span_label(cx.span(attr), "rustdoc does not generate documentation for macro invocations");
    diag.help("to document an item produced by a macro, the macro must produce the documentation as part of its expansion");
    diag
}

/// The node of `owner` two levels up, when it is of `kind`: a field's struct pattern or
/// expression, which the early lint pass checks the field's doc comments on.
fn grandparent(owner: &Owner<'_>, kind: SyntaxKind) -> Option<InFile<SyntaxNodePtr>> {
    let node = owner.node.value.parent()?.parent().filter(|it| it.kind() == kind)?;
    Some(InFile::new(owner.node.file_id, ast::ptr(node)))
}

/// Whether `node` is a block rustc's AST has as a `Block`, not an expression: a function's
/// body, the `then` of an `if`, a loop's body, a `let .. else`'s block.
fn is_block(node: ast::Node<'_>) -> bool {
    if node.kind() != SyntaxKind::BLOCK_EXPR {
        return false;
    }
    let Some(parent) = node.parent() else { return false };
    let is = |block: Option<ast::BlockExpr<'_>>| block.is_some_and(|it| it.syntax() == node);
    match parent.kind() {
        SyntaxKind::FN | SyntaxKind::LET_ELSE => true,
        SyntaxKind::IF_EXPR => is(ast::IfExpr::cast(parent).and_then(|it| it.then_branch())),
        SyntaxKind::WHILE_EXPR => is(ast::WhileExpr::cast(parent).and_then(|it| it.loop_body())),
        SyntaxKind::FOR_EXPR => is(ast::ForExpr::cast(parent).and_then(|it| it.loop_body())),
        SyntaxKind::LOOP_EXPR => is(ast::LoopExpr::cast(parent).and_then(|it| it.loop_body())),
        _ => false,
    }
}

/// The `mod` items module files are declared by, found once when a lint needs one.
#[derive(Default)]
pub(in crate::lints::syntax::attrs) struct Modules(Option<HashMap<HirFileId, InFile<SyntaxNodePtr>>>);

impl Modules {
    fn declaration(&mut self, cx: &Cx<'_, '_>, file: HirFileId) -> Option<InFile<SyntaxNodePtr>> {
        let declared = self.0.get_or_insert_with(|| {
            let mut declared = HashMap::default();
            for module in crate::checks::modules(cx) {
                if let Some(file) = module.as_source_file_id(cx.db)
                    && let Some(declaration) = module.declaration_source_view(cx.db)
                {
                    declared.insert(HirFileId::from(file), declaration.map(|it| ast::ptr(it.syntax())));
                }
            }
            declared
        });
        declared.get(&file).cloned()
    }
}

/// The node rustc's expansion buffers the lints of the macro call `call`'s attributes at
/// (`current_expansion.lint_node_id`): the innermost node around the call that the invocation
/// collector gives an id of its own, through the expansions the call is in. A module file's
/// root is its `mod` item.
pub(in crate::lints::syntax::attrs) fn lint_node(cx: &Cx<'_, '_>, call: InFile<ast::Node<'_>>, modules: &mut Modules) -> InFile<SyntaxNodePtr> {
    let mut at = call;
    loop {
        // `at` itself is a macro call (or what an attribute macro or derive is on): no node
        // of rustc's AST, it is expanded away.
        let mut root = at.value;
        for node in at.value.ancestors().skip(1) {
            if has_id(node) {
                if node.kind() == SyntaxKind::SOURCE_FILE
                    && let Some(declaration) = modules.declaration(cx, at.file_id)
                {
                    return declaration;
                }
                return InFile::new(at.file_id, ast::ptr(node));
            }
            root = node;
        }
        match at.file_id.macro_file() {
            Some(expansion) => at = expansion.loc(cx.db).to_node_view(cx.db),
            None => return InFile::new(at.file_id, ast::ptr(root)),
        }
    }
}

/// Whether rustc's invocation collector gives `node` a `NodeId` of its own (`assign_id!`):
/// items, statements but those of a trailing expression or a macro call, expressions,
/// patterns, types, arms, fields, parameters, generic parameters, variants, where predicates
/// and the crate.
fn has_id(node: ast::Node<'_>) -> bool {
    match node.kind() {
        SyntaxKind::MACRO_CALL | SyntaxKind::MACRO_EXPR | SyntaxKind::MACRO_PAT | SyntaxKind::MACRO_TYPE => false,
        SyntaxKind::SOURCE_FILE
        | SyntaxKind::LET_STMT
        | SyntaxKind::VARIANT
        | SyntaxKind::RECORD_FIELD
        | SyntaxKind::TUPLE_FIELD
        | SyntaxKind::RECORD_PAT_FIELD
        | SyntaxKind::RECORD_EXPR_FIELD
        | SyntaxKind::PARAM
        | SyntaxKind::SELF_PARAM
        | SyntaxKind::TYPE_PARAM
        | SyntaxKind::LIFETIME_PARAM
        | SyntaxKind::CONST_PARAM
        | SyntaxKind::MATCH_ARM
        | SyntaxKind::WHERE_PRED => true,
        SyntaxKind::EXPR_STMT => ast::ExprStmt::cast(node).is_some_and(|it| {
            it.semicolon_token().is_some() && !matches!(it.expr(), Some(ast::Expr::MacroExpr(_)))
        }),
        SyntaxKind::BLOCK_EXPR => !is_block(node),
        kind => ast::Item::can_cast(kind) || ast::Expr::can_cast(kind) || ast::Pat::can_cast(kind) || ast::Type::can_cast(kind),
    }
}
