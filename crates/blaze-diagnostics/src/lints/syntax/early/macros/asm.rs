//! `bad_asm_style`, the lint `asm!`, `global_asm!` and `naked_asm!` buffer as they expand: a
//! template naming `.intel_syntax` or `.att_syntax` on x86. The label lints of the late pass
//! `AsmLabels` are `lints/bodies/calls/methods/asm_labels.rs`.
//!
//! Follows rustc's `compiler/rustc_parse/src/parser/asm.rs` (`parse_asm_operand`: the operands
//! `global_asm!` and `naked_asm!` refuse), `compiler/rustc_builtin_macros/src/asm.rs`
//! (`validate_asm_args`: what stops before the expansion; `expand_preparsed_asm`:
//! `template_strs`, `find_span` and the `BAD_ASM_STYLE` lints, buffered at
//! `current_expansion.lint_node_id`), `compiler/rustc_builtin_macros/src/util.rs`
//! (`expr_to_spanned_string`), `compiler/rustc_builtin_macros/src/diagnostics.rs`
//! (`AvoidIntelSyntax`, `AvoidAttSyntax`), `compiler/rustc_expand/src/expand.rs`
//! (`InvocationCollector`, `assign_id!`: the nodes that are a call's `lint_node_id`) and
//! `compiler/rustc_target/src/asm/mod.rs` (`InlineAsmArch::from_arch`) (MIT or Apache-2.0).

use hir::{InFile, MacroCallId};
use rustc_parse_format as parse_format;
use syntax::{
    SyntaxKind, SyntaxNodePtr, T,
    view::{self as ast, AstNode as _, AstToken as _, HasLoopBody as _, IsString as _},
};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::{Phase, break_outside_of_loop::up},
    model::{Diag, Span},
};

/// rustc's `AsmMacro`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AsmMacro {
    Asm,
    GlobalAsm,
    NakedAsm,
}

/// What of a call's arguments decides whether rustc expands it and how it reads its templates.
#[derive(Default)]
struct Args {
    raw: bool,
    noreturn: bool,
    /// An `out`, `lateout`, `inout` or `inlateout` operand.
    outputs: bool,
    labels: bool,
    clobber_abi: bool,
    /// What `parse_asm_operand` refuses whatever `cfg` says of it: an `in`, `out`, `lateout`,
    /// `inout`, `inlateout` or `label` operand of `global_asm!` or `naked_asm!`, or `_` going
    /// in.
    unparsed: bool,
}

impl Args {
    fn of(tree: &Tree<'_>, asm: ast::AsmExpr<'_>, kind: AsmMacro) -> Args {
        let mut args = Args::default();
        for piece in asm.asm_pieces() {
            let on = !tree.skipped(piece.syntax().text_range());
            match piece {
                ast::AsmPiece::AsmOptions(options) if on => {
                    for option in options.asm_options() {
                        match option.syntax().first_token().map(|it| it.kind()) {
                            Some(T![raw]) => args.raw = true,
                            Some(T![noreturn]) => args.noreturn = true,
                            _ => {}
                        }
                    }
                }
                ast::AsmPiece::AsmClobberAbi(_) if on => args.clobber_abi = true,
                ast::AsmPiece::AsmOperandNamed(named) => match named.asm_operand() {
                    Some(ast::AsmOperand::AsmRegOperand(op)) => {
                        let goes_in = op.asm_dir_spec().is_some_and(|dir| {
                            dir.in_token().is_some()
                                || dir.inout_token().is_some()
                                || dir.inlateout_token().is_some()
                        });
                        let underscore = matches!(
                            op.asm_operand_expr().and_then(|it| it.in_expr()),
                            Some(ast::Expr::UnderscoreExpr(_))
                        );
                        args.unparsed |= kind != AsmMacro::Asm || (goes_in && underscore);
                        let output = op.asm_dir_spec().is_some_and(|dir| dir.in_token().is_none());
                        args.outputs |= on && output;
                    }
                    Some(ast::AsmOperand::AsmLabel(_)) => {
                        args.unparsed |= kind != AsmMacro::Asm;
                        args.labels |= on;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        args
    }

    /// Whether rustc's parser or `validate_asm_args` refuses the call, so that it is never
    /// expanded.
    fn refused(&self, kind: AsmMacro) -> bool {
        self.unparsed
            || (kind == AsmMacro::Asm && self.noreturn && self.outputs && !self.labels)
            || (kind != AsmMacro::Asm && self.clobber_abi)
    }
}

/// Whether the target's `sess.asm_arch` (`InlineAsmArch::from_arch` of its `target_arch`) is
/// x86 or x86_64: elsewhere the crate's `asm!`, `global_asm!` and `naked_asm!` calls give no
/// lint.
pub(super) fn x86(walk: &Walk<'_>) -> bool {
    walk.cfg.get_cfg_values("target_arch").next().is_some_and(|arch| matches!(arch.as_str(), "x86" | "x86_64"))
}

/// The lints of `tree`, the expansion of `call`, a call of `asm!`, `global_asm!` or
/// `naked_asm!` on x86 ([`x86`]).
pub(super) fn lints<'db>(cx: &Cx<'_, 'db>, tree: &Tree<'db>, call: MacroCallId, out: &mut Vec<Emitted>) {
    let asm = tree.of_kind(|kind| kind == SyntaxKind::ASM_EXPR).next();
    let Some(asm) = asm.and_then(ast::AsmExpr::cast) else { return };
    asm_lints(cx, tree, call, asm, out);
}

fn asm_lints<'db>(
    cx: &Cx<'_, 'db>,
    tree: &Tree<'db>,
    call: MacroCallId,
    asm: ast::AsmExpr<'db>,
    emitted: &mut Vec<Emitted>,
) {
    let kind = if asm.global_asm_token().is_some() {
        AsmMacro::GlobalAsm
    } else if asm.naked_asm_token().is_some() {
        AsmMacro::NakedAsm
    } else {
        AsmMacro::Asm
    };
    let args = Args::of(tree, asm, kind);
    if args.refused(kind) {
        return;
    }
    let mut lint_node = None;
    for expr in asm.template() {
        if tree.skipped(expr.syntax().text_range()) {
            continue;
        }
        let Some((value, style)) = string_of(cx, expr) else { return };
        for (needle, message) in [
            (".intel_syntax", "avoid using `.intel_syntax`, Intel syntax is the default"),
            (".att_syntax", "avoid using `.att_syntax`, prefer using `options(att_syntax)` instead"),
        ] {
            if !value.contains(needle) {
                continue;
            }
            let span = cx.span(InFile::new(tree.file, expr.syntax().text_range()));
            let at = *lint_node
                .get_or_insert_with(|| lint_node_of(cx, call.loc(cx.db).to_node_view(cx.db)));
            let end: fn(char) -> bool = |c| matches!(c, '\n' | ';' | '\\' | '"');
            let found = cx.snippet(&span).and_then(|it| inner(it, needle, end));
            let mut diag = Diag::lint("bad_asm_style", message, within(&span, found));
            source_fact(cx, &mut diag, at);
            emitted.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
        }
        if !args.raw && !parses(&value, style) {
            return;
        }
    }
}

/// rustc's `expr_to_spanned_string` of a template once the macros it is a call of are expanded:
/// its string and, when it is raw, its number of `#`s; `None` for what is not a string.
fn string_of<'db>(cx: &Cx<'_, 'db>, expr: ast::Expr<'db>) -> Option<(String, Option<usize>)> {
    match expr {
        ast::Expr::Literal(literal) => match literal.kind() {
            ast::LiteralKind::String(it) => {
                let value = it.value().ok()?.into_owned();
                let hashes =
                    it.is_raw().then(|| it.text()[1..].bytes().take_while(|&b| b == b'#').count());
                Some((value, hashes))
            }
            _ => None,
        },
        ast::Expr::MacroExpr(it) => {
            let expanded = cx.vsema.expand_macro_call(it.macro_call()?)?;
            string_of(cx, ast::Expr::cast(expanded.value)?)
        }
        _ => None,
    }
}

/// Whether `rustc_parse_format` reads `template` as an asm template without an error.
fn parses(template: &str, style: Option<usize>) -> bool {
    let mut parser =
        parse_format::Parser::new(template, style, None, false, parse_format::ParseMode::InlineAsm);
    while parser.next().is_some() {
        if !parser.errors.is_empty() {
            break;
        }
    }
    parser.errors.is_empty()
}

/// `find_span`: from where `needle` first is in a template's source text to the first
/// character after it that `end` holds for, else to the last character but one.
fn inner(snippet: &str, needle: &str, end: fn(char) -> bool) -> Option<(usize, usize)> {
    let pos = snippet.find(needle)?;
    let rest = &snippet[pos..];
    Some((pos, pos + rest.find(end).unwrap_or(rest.len() - 1)))
}

/// `Span::from_inner` of `span` for what [`inner`] found, else `span`.
fn within(span: &Span, inner: Option<(usize, usize)>) -> Span {
    match inner {
        Some((lo, hi)) => span.with_lo(span.lo + lo as u32).with_hi(span.lo + hi as u32),
        // clone: a lint's span, for a lint found
        None => span.clone(),
    }
}

/// rustc's `current_expansion.lint_node_id` at the macro call `call`: the innermost node around
/// it `InvocationCollector` gives an id with `assign_id!`, through the calls of the macros it
/// is in to the crate's root.
fn lint_node_of<'db>(cx: &Cx<'_, 'db>, call: InFile<ast::Node<'db>>) -> InFile<SyntaxNodePtr> {
    let mut last = call;
    for it in up(cx, call).skip(1) {
        if has_lint_id(it.value) {
            return it.map(ast::ptr);
        }
        last = it;
    }
    last.map(ast::ptr)
}

/// Whether rustc's node for `node` is one `assign_id!` gives an id: the crate, an item, a
/// variant, a where predicate, a field, a field of a pattern or of a struct expression, a
/// parameter, a generic parameter, a match arm, a `let` or a statement ending in `;`, a type, a
/// pattern or an expression; never a macro call, which its expansion replaces.
fn has_lint_id(node: ast::Node<'_>) -> bool {
    let kind = node.kind();
    if ast::MacroCall::can_cast(kind)
        || ast::MacroExpr::can_cast(kind)
        || ast::MacroType::can_cast(kind)
        || ast::MacroPat::can_cast(kind)
    {
        return false;
    }
    if let Some(stmt) = ast::ExprStmt::cast(node) {
        return stmt.semicolon_token().is_some()
            && !matches!(stmt.expr(), Some(ast::Expr::MacroExpr(_)));
    }
    if let Some(block) = ast::BlockExpr::cast(node) {
        return is_expr(block);
    }
    ast::SourceFile::can_cast(kind)
        || ast::Item::can_cast(kind)
        || ast::Variant::can_cast(kind)
        || ast::WherePred::can_cast(kind)
        || ast::RecordField::can_cast(kind)
        || ast::TupleField::can_cast(kind)
        || ast::RecordPatField::can_cast(kind)
        || ast::RecordExprField::can_cast(kind)
        || ast::Param::can_cast(kind)
        || ast::SelfParam::can_cast(kind)
        || ast::GenericParam::can_cast(kind)
        || ast::MatchArm::can_cast(kind)
        || ast::LetStmt::can_cast(kind)
        || ast::Type::can_cast(kind)
        || ast::Pat::can_cast(kind)
        || ast::Expr::can_cast(kind)
}

/// Whether `block` is an expression to rustc, not the `Block` of a function, a loop, the
/// branch an `if` takes, a `let .. else` or an asm `label`.
fn is_expr(block: ast::BlockExpr<'_>) -> bool {
    let Some(parent) = block.syntax().parent() else { return true };
    let is = |it: Option<ast::BlockExpr<'_>>| it == Some(block);
    let kind = parent.kind();
    if ast::Fn::can_cast(kind) || ast::LetElse::can_cast(kind) || ast::AsmLabel::can_cast(kind) {
        return false;
    }
    if let Some(it) = ast::IfExpr::cast(parent) {
        return !is(it.then_branch());
    }
    if let Some(it) = ast::LoopExpr::cast(parent) {
        return !is(it.loop_body());
    }
    if let Some(it) = ast::WhileExpr::cast(parent) {
        return !is(it.loop_body());
    }
    if let Some(it) = ast::ForExpr::cast(parent) {
        return !is(it.loop_body());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_span_runs_from_the_needle_to_its_end() {
        let style: fn(char) -> bool = |c| matches!(c, '\n' | ';' | '\\' | '"');
        let intel = inner("\".intel_syntax noprefix; nop\"", ".intel_syntax", style);
        assert_eq!(intel, Some((1, 23)));
        assert_eq!(inner("\".att_syntax\"", ".att_syntax", style), Some((1, 12)));
        assert_eq!(inner("\"\\x2eatt_syntax\"", ".att_syntax", style), None);
    }
}
