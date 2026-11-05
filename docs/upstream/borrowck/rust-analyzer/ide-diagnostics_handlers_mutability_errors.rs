use ide_db::source_change::SourceChange;
use ide_db::text_edit::TextEdit;
use syntax::{AstNode, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, T, ast};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext, fix};

// Diagnostic: need-mut
//
// This diagnostic is triggered on mutating an immutable variable.
pub(crate) fn need_mut(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::NeedMut<'_>,
) -> Option<Diagnostic> {
    let root = d.span.file_id.parse_or_expand(ctx.sema.db);
    let node = d.span.value.to_node(&root);
    let mut span = d.span;
    if let Some(parent) = node.parent()
        && ast::BinExpr::can_cast(parent.kind())
    {
        // In case of an assignment, the diagnostic is provided on the variable name.
        // We want to expand it to include the whole assignment, but only when this
        // is an ordinary assignment, not a destructuring assignment. So, the direct
        // parent is an assignment expression.
        span = d.span.with_value(SyntaxNodePtr::new(&parent));
    };

    let fixes = (|| {
        if d.local.is_ref(ctx.sema.db) {
            // There is no simple way to add `mut` to `ref x` and `ref mut x`
            return None;
        }
        let file_id = span.file_id.file_id()?;
        let mut edit_builder = TextEdit::builder();
        let use_range = span.value.text_range();
        for source in d.local.sources(ctx.sema.db) {
            let Some(ast) = source.name() else { continue };
            // FIXME: macros
            edit_builder.insert(ast.value.syntax().text_range().start(), "mut ".to_owned());
        }
        let edit = edit_builder.finish();
        Some(vec![fix(
            "add_mut",
            "Change it to be mutable",
            SourceChange::from_text_edit(file_id.file_id(ctx.sema.db), edit),
            use_range,
        )])
    })();

    Some(
        Diagnostic::new_with_syntax_node_ptr(
            ctx,
            // FIXME: `E0384` is not the only error that this diagnostic handles
            DiagnosticCode::RustcHardError("E0384"),
            format!(
                "cannot mutate immutable variable `{}`",
                d.local.name(ctx.sema.db).display(ctx.sema.db, ctx.edition)
            ),
            span,
        )
        .stable()
        .with_fixes(fixes),
    )
}

// Diagnostic: unused-mut
//
// This diagnostic is triggered when a mutable variable isn't actually mutated.
pub(crate) fn unused_mut(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::UnusedMut<'_>,
) -> Option<Diagnostic> {
    let ast = d.local.primary_source(ctx.sema.db).syntax_ptr();
    let fixes = (|| {
        let file_id = ast.file_id.file_id()?;
        let mut edit_builder = TextEdit::builder();
        let use_range = ast.value.text_range();
        for source in d.local.sources(ctx.sema.db) {
            let ast = source.syntax();
            let Some(mut_token) = token(ast, T![mut]) else { continue };
            edit_builder.delete(mut_token.text_range());
            if let Some(token) = mut_token.next_token()
                && token.kind() == SyntaxKind::WHITESPACE
            {
                edit_builder.delete(token.text_range());
            }
        }
        let edit = edit_builder.finish();
        Some(vec![fix(
            "remove_mut",
            "Remove unnecessary `mut`",
            SourceChange::from_text_edit(file_id.file_id(ctx.sema.db), edit),
            use_range,
        )])
    })();
    Some(
        Diagnostic::new_with_syntax_node_ptr(
            ctx,
            DiagnosticCode::RustcLint("unused_mut"),
            "variable does not need to be mutable",
            ast,
        )
        // Not supporting `#[allow(unused_mut)]` in proc macros leads to false positive, hence not stable.
        .with_fixes(fixes),
    )
}

pub(super) fn token(parent: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    parent.children_with_tokens().filter_map(|it| it.into_token()).find(|it| it.kind() == kind)
}

#[cfg(test)]
