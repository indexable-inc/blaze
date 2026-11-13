//! `gapped_literal`: a string literal one line of which holds a run of eight or more spaces
//! between two characters of its text: a lost `\` line continuation, which prints the message
//! with a gap in it (agents/annoyances/lost-line-continuations.json: two lanes' scripts broke
//! the continuations of messages, and the gaps shipped).
//!
//! A `\` at the end of a literal's line skips the newline and the next line's leading
//! whitespace, so `"a \` and `    b"` on the next line is `"a b"`. When the `\` and the newline
//! are lost the two lines join, and the value keeps every space of the indentation.
//!
//! A finding is a literal written in the file (`".."`, `b".."`, `c".."`; a raw literal is never
//! one, its text being exactly what it means), in code or in a macro call's input, tests
//! included, with on one of its source lines a run of [`RUN`] or more spaces that has a
//! character of the literal's text before it and after it on that line. Neither neighbour may
//! be whitespace, the opening quote, an escape `\n`, `\r` or `\t`, or a format placeholder
//! `{..}` (`{{` and `}}` are plain text): the run is then a column of a table or of a picture,
//! not a join. Alignment on purpose is kept with `#[expect(blaze::gapped_literal, reason =
//! "..")]` on the item that holds it, or written as a raw literal.
//!
//! The fix puts the `\` and the newline back: the run becomes ` \`, a newline and the run less
//! one space, so the value has one space where the run was. It is machine-applicable when the
//! run less one space is a whole indentation (a multiple of four, at least the line's own), the
//! shape a joined continuation leaves; otherwise it may be wrong.

use syntax::{
    NodeOrToken, SyntaxKind, TextRange, TextSize,
    view::{self as ast, AstNode as _},
};

use super::Lint;
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::GappedLiteral;

/// How many spaces in a row inside a literal's line are a gap.
const RUN: usize = 8;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, node: ast::Node<'t>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    literals(cx, node, |cx, token, range| {
        let source = cx.snippet(range);
        gaps(source, |start, len| report(cx, token, range, start, len));
    });
}

/// Calls `f` with each string literal token (`".."`, `b".."`, `c".."`, raw or not) the walk
/// sees at `node` and where it is written in [`Cx::text`]: the node's own tokens, or for a
/// macro call every token of its input, which the walk does not enter. Each literal written in
/// a file is seen once over the file's walks: none of an expansion of a call (its input was
/// seen at the call) or of a derive (it writes no literal of the file), none a macro made up.
pub(super) fn literals<'t>(
    cx: &mut Cx<'_, 't>,
    node: ast::Node<'t>,
    mut f: impl FnMut(&mut Cx<'_, 't>, ast::Token<'t>, TextRange),
) {
    if cx.in_bang_call() || cx.expansion().is_some() || !cx.is_written(node) {
        return;
    }
    let mut each = |cx: &mut Cx<'_, 't>, it: NodeOrToken<ast::Node<'t>, ast::Token<'t>>| {
        let NodeOrToken::Token(token) = it else { return };
        if !matches!(token.kind(), SyntaxKind::STRING | SyntaxKind::BYTE_STRING | SyntaxKind::C_STRING) {
            return;
        }
        if let Some(range) = cx.token_range(token) {
            f(cx, token, range);
        }
    };
    match ast::MacroCall::cast(node).and_then(|it| it.token_tree()) {
        Some(tt) => {
            for it in tt.syntax().descendants_with_tokens() {
                each(cx, it);
            }
        }
        None => {
            for it in node.children_with_tokens() {
                each(cx, it);
            }
        }
    }
}

/// What a character of a literal's line is to a run of spaces beside it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    /// The line's start (the opening quote, or a newline in the literal).
    Start,
    /// Whitespace.
    Space,
    /// An escape `\n`, `\r` or `\t`, a placeholder `{..}`, or a `\` that ends the line.
    Break,
    /// Any other character of the text.
    Plain,
}

/// Calls `f` with the offset in `source`, a literal as written, and the length of each run of
/// [`RUN`] or more spaces of one of its lines that has a plain character on both sides; none
/// for a raw literal.
fn gaps(source: &str, mut f: impl FnMut(usize, usize)) {
    let Some(open) = source.find('"') else { return };
    if source[..open].contains('r') {
        return;
    }
    let end = match source.len() > open + 1 && source.ends_with('"') {
        true => source.len() - 1,
        false => source.len(),
    };
    let bytes = source.as_bytes();
    let mut at = open + 1;
    let mut last = Side::Start;
    while at < end {
        match bytes[at] {
            b'\n' => {
                last = Side::Start;
                at += 1;
            }
            b' ' => {
                let len = bytes[at..end].iter().take_while(|it| **it == b' ').count();
                let next = side_at(bytes, at + len, end);
                if len >= RUN && last == Side::Plain && next == Side::Plain {
                    f(at, len);
                }
                last = Side::Space;
                at += len;
            }
            _ => {
                let (side, width) = token_at(bytes, at, end);
                last = side;
                at += width;
            }
        }
    }
}

/// What the character at `at` of `bytes`, before `end`, is to a run that ends before it.
fn side_at(bytes: &[u8], at: usize, end: usize) -> Side {
    if at >= end {
        return Side::Start;
    }
    match bytes[at] {
        b'\n' => Side::Start,
        b' ' | b'\t' | b'\r' => Side::Space,
        _ => token_at(bytes, at, end).0,
    }
}

/// What the text at `at` of `bytes` is, and how many bytes it takes: an escape, a placeholder,
/// a doubled brace, or one plain byte.
fn token_at(bytes: &[u8], at: usize, end: usize) -> (Side, usize) {
    let next = bytes.get(at + 1).copied().filter(|_| at + 1 < end);
    match (bytes[at], next) {
        (b'\\', Some(b'n' | b'r' | b't')) => (Side::Break, 2),
        (b'\\', None | Some(b'\n' | b'\r')) => (Side::Break, 1),
        (b'\\', Some(_)) => (Side::Plain, 2),
        (b'{', Some(b'{')) | (b'}', Some(b'}')) => (Side::Plain, 2),
        (b'{', _) => match bytes[at..end].iter().take_while(|it| **it != b'\n').position(|it| *it == b'}') {
            Some(close) => (Side::Break, close + 1),
            None => (Side::Plain, 1),
        },
        (b'\t' | b'\r', _) => (Side::Space, 1),
        _ => (Side::Plain, 1),
    }
}

fn report<'t>(cx: &mut Cx<'_, 't>, token: ast::Token<'t>, range: TextRange, start: usize, len: usize) {
    let Ok(offset) = u32::try_from(start) else { return };
    let Ok(width) = u32::try_from(len) else { return };
    let run = TextRange::at(range.start() + TextSize::new(offset), TextSize::new(width));
    let indent = indentation(cx.text(), run.start());
    let continued = len - 1;
    let applicability = match continued % 4 == 0 && continued >= indent {
        true => Applicability::MachineApplicable,
        false => Applicability::MaybeIncorrect,
    };
    // alloc: the fix's text, once per finding
    let fix = format!(" \\\n{}", " ".repeat(continued));
    let message = format!(
        "{len} spaces in a row inside one line of this literal: a lost `\\` line continuation keeps them in its value"
    );
    let span = cx.span(run);
    let fix_span = cx.span(run);
    let at = token.parent().unwrap_or(cx.root());
    cx.blaze(LINT, at, span, message)
        .note(
            "a `\\` at the end of a literal's line skips the newline and the next line's leading spaces; without it \
             the lines join and the message prints with a gap",
        )
        .multipart_suggestion(
            "put the line continuation back",
            // alloc: the fix's one part
            vec![(fix_span, fix)],
            applicability,
        )
        .help(LINT.help())
        .help(format!("for further information visit {}", LINT.docs_url()));
}

/// The leading spaces of the line of `text` that `at` is on.
fn indentation(text: &str, at: TextSize) -> usize {
    let at = usize::from(at).min(text.len());
    let start = text[..at].rfind('\n').map_or(0, |it| it + 1);
    text[start..at].bytes().take_while(|it| *it == b' ').count()
}

#[cfg(test)]
mod tests {
    use super::gaps;
    use crate::tests::lint;

    const FOUND: &str = "spaces in a row inside one line of this literal";

    fn found(main: &str) -> String {
        let fixture = format!("//- /src/lib.rs crate:main\n{}", main.trim_start_matches('\n'));
        lint(&fixture, "-A clippy::all --warn=blaze::gapped_literal")
    }

    /// The runs `gaps` finds in `source`, as (offset, length).
    fn runs(source: &str) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        gaps(source, |at, len| out.push((at, len)));
        out
    }

    /// The bug in miniature: two lines of a message joined when their `\` was lost, in a
    /// `const`, in a body and in a macro call's input.
    #[test]
    fn a_lost_continuation_is_found() {
        let out = found(
            r#"
pub const USAGE: &str = "blaze check runs the lints             and stops at the first error";
pub fn says(n: u32) -> u32 {
    let _ = "the build is done:            nothing reran";
    if n > 9 {
        panic!("{n} items is more than the            queue holds");
    }
    n
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 3, "{out}");
        assert!(out.contains("13 spaces in a row"), "{out}");
        assert!(out.contains("put the line continuation back"), "{out}");
    }

    /// Not a gap: a run of fewer than eight, one after an escape `\n` or a placeholder, the
    /// indentation after a kept `\`, trailing spaces, a run at the line's start, and any raw
    /// literal.
    #[test]
    fn alignment_and_kept_continuations_are_left_alone() {
        let out = found(
            r#"
pub const SHORT: &str = "a column       apart";
pub const AFTER_ESCAPE: &str = "usage:\n        blaze check";
pub fn table(name: &str) -> String {
    format!("{name}        {name}")
}
pub const KEPT: &str = "the first half of a message \
                        and the rest of it";
pub const TRAILING: &str = "ends here        ";
pub const RAW: &str = r"a raw table        keeps its columns";
pub const BYTES: &[u8] = br"raw bytes        too";
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
    }

    /// A table aligned on purpose is kept by an expectation on the item that holds it, which
    /// the finding fulfils.
    #[test]
    fn an_expectation_on_the_item_is_fulfilled() {
        let out = found(
            r#"
#[expect(blaze::gapped_literal, reason = "the usage table is aligned in two columns")]
pub fn usage() -> &'static str {
    "check        lint the workspace"
}
"#,
        );
        assert_eq!(out.matches(FOUND).count(), 0, "{out}");
        assert!(!out.contains("unfulfilled"), "{out}");
    }

    /// The scan itself: where a run is, for the shapes the lint's rule names.
    #[test]
    fn the_scan_finds_runs_between_plain_characters() {
        assert_eq!(runs(r#""a        b""#), vec![(2, 8)]);
        assert_eq!(runs(r#"b"a        b""#), vec![(3, 8)]);
        assert_eq!(runs(r#""a\t        b""#), vec![]);
        assert_eq!(runs(r#""a        \nb""#), vec![]);
        assert_eq!(runs(r#""{{        }}""#), vec![(3, 8)]);
        assert_eq!(runs(r#""{a}        b""#), vec![]);
        assert_eq!(runs(r#""a\"        b""#), vec![(4, 8)]);
        assert_eq!(runs("\"a \\\n        b\""), vec![]);
        assert_eq!(runs(r#"r"a        b""#), vec![]);
    }
}
