//! `undocumented_unsafe_blocks`: an `unsafe` block or `unsafe impl` with no `// SAFETY:`
//! comment on the lines just above it.
//!
//! Follows the `undocumented_unsafe_blocks` part of clippy's
//! `clippy_lints/src/undocumented_unsafe_blocks.rs` (`check_block`, `check_item`,
//! `text_has_safety_comment` and the parent walks) (MIT or Apache-2.0), ported from what is
//! known of its source at rust-lang/rust's nightly of 2026-10: no reference checkout of clippy
//! was at hand. `accept-comment-above-statement` and `accept-comment-above-attributes` (both on
//! by default) are clippy's.
//!
//! Where it differs: clippy searches above a block from the end of the item before the body's
//! item; a body's lints see only the body's own tree, so the search starts at the tree's first
//! line (a one-line function with the comment above it is linted). The first item of a module
//! file is searched from the file's start. An `unsafe impl` that a macro was handed is left
//! alone, as clippy leaves those it cannot place.

use syntax::{
    Direction, TextRange, TextSize,
    view::{self as ast, AstNode as _, BinaryOp},
};

use crate::check::Cx;

const LINT: super::Implemented = super::Implemented::UndocumentedUnsafeBlocks;

const HELP: &str = "consider adding a safety comment on the preceding line";

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on(LINT) {
        return;
    }
    if let Some(block) = ast::BlockExpr::cast(node) {
        check_block(cx, block);
    } else if let Some(imp) = ast::Impl::cast(node) {
        check_impl(cx, imp);
    }
}

fn check_block<'t>(cx: &mut Cx<'_, 't>, block: ast::BlockExpr<'t>) {
    let Some(ast::BlockModifier::Unsafe(_)) = block.modifier() else { return };
    let node = block.syntax();
    if !cx.is_written(node) {
        return;
    }
    let Some(range) = block_range(cx, block) else { return };
    let text = cx.text();
    // clippy's `is_unsafe_from_proc_macro`: the source at the block is not `unsafe`.
    if !text[usize::from(range.start())..].starts_with("unsafe") {
        return;
    }
    if has_safety_comment(between_lines(text, 0, usize::from(range.start()))) {
        return;
    }
    if cx.conf().bool("accept-comment-above-statement", true)
        && let Some(start) = parent_start(cx, node)
        && has_safety_comment(between_lines(text, 0, usize::from(start)))
    {
        return;
    }
    let span = cx.span(until_newline(text, range));
    cx.lint(LINT, node, span, "unsafe block missing a safety comment").help(HELP);
}

/// clippy's `check_has_no_safety_comment` of an item, with `item_has_safety_comment`.
fn check_impl<'t>(cx: &mut Cx<'_, 't>, imp: ast::Impl<'t>) {
    if imp.unsafe_token().is_none() {
        return;
    }
    let node = imp.syntax();
    if !cx.in_file() {
        return;
    }
    let Some(whole) = cx.range(node) else { return };
    // An item's span starts after its attributes, doc comments and the comments before it.
    let start = start_of(node, false);
    let range = TextRange::new(start.max(whole.start()), whole.end());
    let text = cx.text();
    if !text[usize::from(range.start())..].starts_with("unsafe") {
        return;
    }
    let Some(comment_start) = comment_start_before_item(cx, node) else { return };
    let between = between_lines(text, usize::from(comment_start), usize::from(range.start()));
    if has_safety_comment(between) {
        return;
    }
    let span = cx.span(until_newline(text, range));
    cx.lint(LINT, node, span, "unsafe impl missing a safety comment").help(HELP);
}

/// Where clippy starts looking for an item's comment (`comment_start_before_item_in_mod` and
/// the statement case of `item_has_safety_comment`): the end of the item before it in its
/// module, else the module's start; in a block, the block's start. `None` where clippy
/// pretends one was found (an item of a trait, an impl or an `extern` block).
fn comment_start_before_item(cx: &Cx<'_, '_>, item: ast::Node<'_>) -> Option<TextSize> {
    let parent = item.parent()?;
    if ast::StmtList::can_cast(parent.kind()) {
        return cx.range(parent).map(|it| it.start());
    }
    let module_start = if ast::SourceFile::can_cast(parent.kind()) {
        TextSize::from(0)
    } else if ast::ItemList::can_cast(parent.kind()) {
        let module = parent.parent().filter(|it| ast::Module::can_cast(it.kind()))?;
        start_of(module, false)
    } else {
        return None;
    };
    let previous =
        item.siblings(Direction::Prev).skip(1).find(|it| ast::Item::can_cast(it.kind()));
    match previous {
        Some(previous) => cx.range(previous).map(|it| it.end()),
        None => Some(module_start),
    }
}

/// clippy's `block_parents_have_safety_comment`: where the statement, `let`, assignment,
/// constant or static the block is part of starts, the comment above which serves too.
fn parent_start<'t>(cx: &Cx<'_, 't>, block: ast::Node<'t>) -> Option<TextSize> {
    let with_attrs = cx.conf().bool("accept-comment-above-attributes", true);
    let mut expr = block;
    // Whether `expr` is an `if` or a `match` in HIR, as `?`, `.await` and the condition of a
    // `while` are desugared to.
    let mut branchy = false;
    while let Some(mut parent) = expr.parent() {
        // HIR has no parentheses, and a call's arguments are its own children.
        if ast::ParenExpr::can_cast(parent.kind()) {
            expr = parent;
            continue;
        }
        if ast::ArgList::can_cast(parent.kind()) {
            parent = parent.parent()?;
        }
        if ast::LetStmt::can_cast(parent.kind())
            || ast::Const::can_cast(parent.kind())
            || ast::Static::can_cast(parent.kind())
        {
            let start = start_of(parent, with_attrs);
            return cx.range(parent).map(|it| start.max(it.start()));
        }
        match ast::Expr::cast(parent) {
            Some(ast::Expr::BinExpr(bin))
                if bin.op_kind() == Some(BinaryOp::Assignment { op: None }) =>
            {
                return eq_start(cx, bin);
            }
            // A `for` loop's span is its desugaring's, which is not of the file; a `while`'s
            // condition is an `if`'s in a `loop`'s block.
            Some(ast::Expr::ForExpr(_) | ast::Expr::WhileExpr(_)) => return None,
            Some(
                ast::Expr::IfExpr(_)
                | ast::Expr::MatchExpr(_)
                | ast::Expr::TryExpr(_)
                | ast::Expr::AwaitExpr(_),
            ) => {
                expr = parent;
                branchy = true;
            }
            Some(_) => {
                expr = parent;
                branchy = false;
            }
            None => break,
        }
    }
    if branchy {
        return None;
    }
    let start = start_of(expr, with_attrs);
    cx.range(expr).map(|it| start.max(it.start()))
}

/// Where the `=` of the assignment `bin` is: HIR's span of `ExprKind::Assign`. A token of an
/// expansion has no place of its own in the file, so there it is taken where the left side ends.
fn eq_start(cx: &Cx<'_, '_>, bin: ast::BinExpr<'_>) -> Option<TextSize> {
    if cx.in_file() {
        return bin.op_token().map(|it| it.text_range().start());
    }
    cx.range(bin.lhs()?.syntax()).map(|it| it.end())
}

/// The range of the written `unsafe` block `block`, in the tree's text: from its `unsafe`, past
/// its attributes and label, as HIR's block span has it.
pub(super) fn block_range(cx: &Cx<'_, '_>, block: ast::BlockExpr<'_>) -> Option<TextRange> {
    let range = cx.range(block.syntax())?;
    let start = match block.unsafe_token() {
        // Only the tree's own tokens have a place in its text.
        Some(token) if cx.in_file() => token.text_range().start().max(range.start()),
        _ => range.start(),
    };
    Some(TextRange::new(start, range.end()))
}

/// Where `node` starts in the tree's text: at its first child that is not whitespace or a
/// comment, and with `with_attrs` false, not an attribute or a doc comment either.
fn start_of(node: ast::Node<'_>, with_attrs: bool) -> TextSize {
    if !with_attrs {
        return ast::written_range(node).start();
    }
    node.children_with_tokens()
        .find(|it| !it.kind().is_trivia())
        .map_or(node.text_range().start(), |it| it.text_range().start())
}

/// `range`, cut at its first line's end (`SourceMap::span_until_char(span, '\n')`).
fn until_newline(text: &str, range: TextRange) -> TextRange {
    let snippet = &text[usize::from(range.start())..usize::from(range.end())];
    let first = snippet.split('\n').next().unwrap_or("").trim_end();
    if first.is_empty() || first.len() == snippet.len() {
        return range;
    }
    let len = u32::try_from(first.len()).unwrap_or(u32::MAX);
    TextRange::at(range.start(), TextSize::from(len))
}

/// The text of the lines after the one `from` is on, up to the line `to` is on: what clippy
/// looks through for the comment. Empty when `to` is on `from`'s line or the next.
fn between_lines(text: &str, from: usize, to: usize) -> &str {
    let to_line = text[..to].rfind('\n').map_or(0, |it| it + 1);
    match text[from..].find('\n') {
        Some(at) if from + at < to_line => &text[from + at + 1..to_line],
        _ => "",
    }
}

/// clippy's `text_has_safety_comment` over `region`, the lines above what is linted: the
/// nearest line that is not blank is a line comment of a run of them, one of which says
/// `SAFETY:` (outside a doc comment's code block), or below the start of a block comment that
/// says `SAFETY:` and that only whitespace follows.
fn has_safety_comment(region: &str) -> bool {
    let base = region.as_ptr() as usize;
    let mut lines =
        region.split_inclusive('\n').rev().map(str::trim_start).filter(|it| !it.is_empty());
    let Some(mut line) = lines.next() else { return false };
    if line.starts_with("//") {
        let mut in_codeblock = false;
        loop {
            if line.trim_start_matches(['/', '!']).trim_start().starts_with("```") {
                in_codeblock = !in_codeblock;
            }
            if !in_codeblock && contains_safety(line) {
                return true;
            }
            match lines.next() {
                Some(next) if next.starts_with("//") => line = next,
                _ => return false,
            }
        }
    }
    loop {
        if line.starts_with("/*") {
            let rest = &region[line.as_ptr() as usize - base..];
            let len = block_comment_len(rest);
            return contains_safety(&rest[..len]) && rest[len..].chars().all(is_whitespace);
        }
        match lines.next() {
            Some(next) => line = next,
            None => return false,
        }
    }
}

/// Whether `text`, in ASCII upper case, holds `SAFETY:`.
fn contains_safety(text: &str) -> bool {
    const NEEDLE: &[u8] = b"SAFETY:";
    text.as_bytes().windows(NEEDLE.len()).any(|it| it.eq_ignore_ascii_case(NEEDLE))
}

/// The length of the block comment `text` starts with, nested ones in it, as rustc's lexer
/// takes it: all of `text` when it is not closed.
fn block_comment_len(text: &str) -> usize {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut at = 0;
    while at < bytes.len() {
        match (bytes[at], bytes.get(at + 1)) {
            (b'/', Some(b'*')) => {
                depth += 1;
                at += 2;
            }
            (b'*', Some(b'/')) => {
                depth -= 1;
                at += 2;
                if depth == 0 {
                    return at;
                }
            }
            _ => at += 1,
        }
    }
    bytes.len()
}

/// rustc's lexer's whitespace (`Pattern_White_Space`).
fn is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n'
            | '\u{B}'
            | '\u{C}'
            | '\r'
            | ' '
            | '\u{85}'
            | '\u{200E}'
            | '\u{200F}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

#[cfg(test)]
mod tests {
    use super::{between_lines, block_comment_len, has_safety_comment};

    #[test]
    fn line_comments() {
        assert!(has_safety_comment("// SAFETY: fine\n"));
        assert!(has_safety_comment("// Safety: fine\n\n"));
        assert!(has_safety_comment("// SAFETY:\n// more of it\n"));
        assert!(!has_safety_comment("// SAFETY: fine\nlet x = 1;\n"));
        assert!(!has_safety_comment("// nothing said\n"));
        assert!(!has_safety_comment(""));
        assert!(!has_safety_comment("/// ```\n/// SAFETY: in a code block\n/// ```\n"));
    }

    #[test]
    fn block_comments() {
        assert!(has_safety_comment("/* SAFETY: fine */\n"));
        assert!(has_safety_comment("/* SAFETY:\n   over lines */\n"));
        assert!(!has_safety_comment("/* SAFETY: fine */\nfoo();\n"));
        assert!(!has_safety_comment("/* nothing said */\n"));
        assert_eq!(block_comment_len("/* a /* b */ c */ d"), 17);
        assert_eq!(block_comment_len("/* open"), 7);
    }

    #[test]
    fn lines_between() {
        let text = "fn f() {\n    // SAFETY: x\n    unsafe {}\n}";
        let to = text.find("unsafe").unwrap();
        assert_eq!(between_lines(text, 0, to), "    // SAFETY: x\n");
        assert_eq!(between_lines(text, 0, 3), "");
        assert_eq!(between_lines("a\nb", 0, 2), "");
    }
}
