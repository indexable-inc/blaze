//! The one rule for line breaks in Rust source, read in place.
//!
//! The language (the Reference, "Input format") turns each CR LF into LF before it tokenises; a
//! string's `\` before a line break continues the line ("String literals"), and a CR that is not
//! part of a CR LF is an error in a string, raw string, character and doc comment. blaze reads the
//! same language without rewriting the text, because records hold positions into the raw bytes
//! and digests of them (rules 25 and 71). So every lexer reads the raw bytes by this rule:
//!
//! - A line break is LF, or CR immediately followed by LF: one break, one or two bytes
//!   ([`line_break_len`]). A CR followed by anything else, or ending the text, is a bare CR
//!   ([`is_bare_cr`]).
//! - Where a line break matters, the CR LF form is that break: a `\` before it continues a
//!   string, a line comment ends at its CR (the comment's text holds no CR), and a raw string's
//!   content holds it.
//! - A value made from text holding a break (a string's or raw string's content, a doc comment's
//!   text) holds one LF for it, never the CR: the language's value is that of the normalised
//!   text ([`without_break_crs`] gives its pieces, no copy).
//! - Positions stay raw. A token's range is its range in the raw bytes; the raw position `p` and
//!   the normalised one are `p` less the CRs of CR LFs before `p`, so two token streams of one
//!   text in both forms map by that offset map, token for token.
//!
//! rustc_lexer (vendored unchanged) already finds token ends by this rule: a line comment ends
//! before the LF (so with its break's CR inside its text) and CR is whitespace. What it leaves to
//! its callers is in this module, and every unescaper, doc comment reader and literal value of
//! the tree calls it: a doc comment's value is [`doc_comment_value`]. Its frontmatter flag
//! `invalid_infostring` reads a CR LF after the opening fence as an infostring; the parser's
//! frontmatter is crate::frontmatter's (cargo's, which takes CR LF), and rustc_lexer lexes the
//! rest with frontmatter not allowed, so that flag is never reached.
//!
//! Left alone: blaze-macro-eval rewrites CR LF to LF before it lexes (`lex.rs`), and blaze-fmt
//! reads and writes line breaks by its own newline style (`formatting/newline_style.rs`); both
//! are old-layer crates removed at the switch to the new item layer.

/// The length of the line break `rest` starts with: 1 for LF, 2 for CR LF, `None` for anything
/// else (a bare CR included).
#[inline]
pub fn line_break_len(rest: &[u8]) -> Option<usize> {
    match rest {
        [b'\n', ..] => Some(1),
        [b'\r', b'\n', ..] => Some(2),
        _ => None,
    }
}

/// Whether `rest` starts with a bare CR: a CR that is not the first byte of a CR LF.
#[inline]
pub fn is_bare_cr(rest: &[u8]) -> bool {
    rest.first() == Some(&b'\r') && rest.get(1) != Some(&b'\n')
}

/// Whether `text[range]` holds a bare CR, a CR at its end judged by the byte after it in `text`
/// (a line comment's last CR is its break's when `text` has LF next).
///
/// A caller that asks per token asks [`has_cr`] of the whole text once first: a text with no CR
/// (nearly every file) then never asks this again. Here the range is searched CR to CR, never
/// byte by byte through a closure (328 self samples in a 2026-11-18 `engine-check` stack sample).
#[inline]
pub fn has_bare_cr(text: &[u8], range: core::ops::Range<usize>) -> bool {
    #[cfg(test)]
    counts::BARE_CR_ASKS.with(|n| n.set(n.get() + 1));
    let (mut at, end) = (range.start, range.end);
    while let Some(i) = text[at..end].iter().position(|&b| b == b'\r') {
        if is_bare_cr(&text[at + i..]) {
            return true;
        }
        at += i + 1;
    }
    false
}

/// Whether `text` holds a CR at all: the one scan a lexer makes of a text for its bare-CR
/// questions ([`has_bare_cr`]), so a text with none asks no token's range.
#[inline]
pub fn has_cr(text: &[u8]) -> bool {
    #[cfg(test)]
    counts::CR_SCANS.with(|n| n.set(n.get() + 1));
    text.contains(&b'\r')
}

/// How often each question was asked on this thread: the tests' measure of the floor (one
/// [`has_cr`] per lexed text; no [`has_bare_cr`] for a text without a CR).
#[cfg(test)]
pub(crate) mod counts {
    use core::cell::Cell;

    std::thread_local! {
        pub(crate) static CR_SCANS: Cell<u64> = const { Cell::new(0) };
        pub(crate) static BARE_CR_ASKS: Cell<u64> = const { Cell::new(0) };
    }
}

/// The pieces of `text` with each CR of a CR LF left out, in order: concatenated, they are the
/// normalised text (each break one LF). A bare CR stays. No piece is empty but the first when
/// `text` starts with CR LF.
pub fn without_break_crs(text: &str) -> impl Iterator<Item = &str> {
    let mut rest = Some(text);
    core::iter::from_fn(move || {
        let now = rest?;
        match now.find("\r\n") {
            Some(cr) => {
                // The LF begins the next piece.
                rest = Some(&now[cr + 1..]);
                Some(&now[..cr])
            }
            None => {
                rest = None;
                Some(now)
            }
        }
    })
}

/// The text of a line comment token as the language reads it: rustc_lexer ends the token before
/// its LF, so a CR LF break leaves its CR as the token's last byte, which is no part of the
/// comment. `followed_by_lf` says whether the byte after the token is LF (it is, unless the
/// token ends the text); a last CR with no LF after it is a bare CR and stays.
#[inline]
pub fn line_comment_text(token: &str, followed_by_lf: bool) -> &str {
    match token.strip_suffix('\r') {
        Some(text) if followed_by_lf => text,
        _ => token,
    }
}

/// The value of the doc comment `token` (`///`, `//!`, `/**` or `/*!`): its text with the
/// markers off (a block's `*/` too, when it has one), in pieces that, concatenated, are the
/// language's value, each line break one LF ([`without_break_crs`]; a block's raw text keeps
/// the CR of each CR LF, a line comment's the CR of its break, [`line_comment_text`] with
/// `followed_by_lf`). `None` when `token` is too short to be a doc comment.
pub fn doc_comment_value(token: &str, followed_by_lf: bool) -> Option<impl Iterator<Item = &str>> {
    let body = match token.starts_with("//") {
        true => line_comment_text(token, followed_by_lf).get(3..)?,
        false => {
            let from_end = if token.starts_with("/*") && token.ends_with("*/") { 2 } else { 0 };
            token.get(3..token.len().checked_sub(from_end)?)?
        }
    };
    Some(without_break_crs(body))
}

#[cfg(test)]
mod tests;
