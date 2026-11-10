//! Where a span of a file is, as a unit's narrow key keeps it ([`crate::Lowered::narrowed`]).
//!
//! A narrow key holds each file's code text ([`crate::code`]): every token with its line and
//! column, and nothing of what moves none (a `//` comment at a line's end, the spaces before a
//! line's end, what is after the last token). So under one narrow key a token's edge is at one
//! line and byte column whatever those say, while its byte offset in the file is not. A
//! diagnostic kept under the narrow key holds its spans' ends as places ([`places`]) and is
//! given their offsets in the file as it is when it is shown ([`offset_of`]), then rendered
//! from that file: an edit the key does not see changes what its quoted lines say, and
//! nothing else.
//!
//! Only a span the code text keeps exactly is kept so: both its ends at an edge of a token,
//! and nothing between them that the code text leaves out or keeps as a shape (a comment of
//! any kind, spaces before a line's end), since a diagnostic may hold text copied from inside
//! its span (a suggestion's replacement). Any other span, one into a comment or to a file's
//! end, makes its record one of the files it quotes, whole.

use span::Edition;
use syntax::{AstNode, SourceFile, SyntaxKind};

/// A place in a text: its line, from 0, and the bytes from that line's start.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Place {
    pub line: u32,
    pub column: u32,
}

/// What a token of a file is to a span over it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class {
    /// A token of the code: its edges are places a narrow key keeps.
    Code,
    /// Whitespace the code text keeps as it is.
    Space,
    /// What the code text leaves out or keeps only the shape of: a comment, or whitespace
    /// with spaces before a line's end.
    Left,
}

/// The places of the ends of each of `spans` (byte offsets `lo..hi`) of `text`, a file of a
/// crate of `edition`; none unless each span is one the code text keeps exactly (the
/// module's docs), or when an offset is past the text or inside a character.
pub fn places(text: &str, edition: Edition, spans: &[(u32, u32)]) -> Option<Vec<(Place, Place)>> {
    let parse = SourceFile::parse(text, edition);
    // alloc: once per file a kept record's diagnostics point into, its tokens
    let mut tokens: Vec<(u32, u32, Class)> = Vec::new();
    let mut next = parse.tree().syntax().first_token();
    while let Some(token) = next {
        let range = token.text_range();
        let class = match token.kind() {
            SyntaxKind::WHITESPACE => match blank_before_a_lines_end(token.text()) {
                true => Class::Left,
                false => Class::Space,
            },
            SyntaxKind::COMMENT
            | SyntaxKind::OUTER_DOC_COMMENT
            | SyntaxKind::INNER_DOC_COMMENT => Class::Left,
            _ => Class::Code,
        };
        tokens.push((u32::from(range.start()), u32::from(range.end()), class));
        next = token.next_token();
    }
    let edge = |at: u32| {
        // The first token that ends at or after `at`: `at` is its end, or the start of it or
        // of the one after it.
        let first = tokens.partition_point(|it| it.1 < at);
        tokens[first..]
            .iter()
            .take(2)
            .any(|it| it.2 == Class::Code && (it.0 == at || it.1 == at))
    };
    // alloc: the places returned
    let mut out = Vec::with_capacity(spans.len());
    for &(lo, hi) in spans {
        if lo > hi || !edge(lo) || !edge(hi) {
            return None;
        }
        let first = tokens.partition_point(|it| it.1 <= lo);
        let inside = tokens[first..].iter().take_while(|it| it.0 < hi);
        if inside.into_iter().any(|it| it.2 == Class::Left) {
            return None;
        }
        out.push((place_of(text, lo)?, place_of(text, hi)?));
    }
    Some(out)
}

/// Whether the whitespace `space` has a space or a tab right before a line's end (`\n` or
/// `\r\n`): what the code text leaves out ([`crate::code`]).
fn blank_before_a_lines_end(space: &str) -> bool {
    let bytes = space.as_bytes();
    bytes.windows(2).any(|pair| {
        matches!(pair[0], b' ' | b'\t') && matches!(pair[1], b'\n' | b'\r')
    })
}

/// The place of byte `offset` of `text`; none for one past its end or inside a character.
pub fn place_of(text: &str, offset: u32) -> Option<Place> {
    let before = text.get(..offset as usize)?;
    let start = before.rfind('\n').map_or(0, |at| at + 1);
    let line = before.as_bytes().iter().filter(|it| **it == b'\n').count();
    Some(Place { line: u32::try_from(line).ok()?, column: u32::try_from(before.len() - start).ok()? })
}

/// The byte offset of `place` in `text`: none when the text has no such line, the line is
/// shorter than its column, or the column is inside a character.
pub fn offset_of(text: &str, place: Place) -> Option<u32> {
    let mut start = 0usize;
    for _ in 0..place.line {
        start += text[start..].find('\n')? + 1;
    }
    let line = text[start..].split('\n').next().unwrap_or("");
    let column = place.column as usize;
    if column > line.len() || !line.is_char_boundary(column) {
        return None;
    }
    u32::try_from(start + column).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "fn f() {\n    let x = 1;\n    g(x);\n}\n";

    fn at(text: &str, what: &str) -> (u32, u32) {
        let lo = text.find(what).unwrap_or_else(|| panic!("no `{what}` in the text"));
        (lo as u32, (lo + what.len()) as u32)
    }

    /// A span of tokens has the same places, and the offsets they give are of the same
    /// tokens, in every text of one code text: with a comment at a line's end, spaces before
    /// one and a comment after the last line.
    #[test]
    fn a_span_of_tokens_is_at_one_place_in_every_text_of_one_code_text() {
        let noted = "fn f() { // opens\n    let x = 1;  \n    g(x); // calls\n}\n// end\n";
        for what in ["x", "let x = 1;", "g(x)", "fn", "}"] {
            let (lo, hi) = at(TEXT, what);
            let placed = places(TEXT, Edition::CURRENT, &[(lo, hi)]).expect(what);
            let (lo_noted, hi_noted) = at(noted, what);
            assert_eq!(
                places(noted, Edition::CURRENT, &[(lo_noted, hi_noted)]).as_ref(),
                Some(&placed),
                "{what}"
            );
            assert_eq!(offset_of(noted, placed[0].0), Some(lo_noted), "{what}");
            assert_eq!(offset_of(noted, placed[0].1), Some(hi_noted), "{what}");
            assert_eq!(offset_of(TEXT, placed[0].0), Some(lo), "{what}");
        }
        // An empty span at a token's edge is one too.
        let (_, end) = at(TEXT, "1");
        assert!(places(TEXT, Edition::CURRENT, &[(end, end)]).is_some());
    }

    /// What the code text does not keep exactly is no place: a span that starts or ends in
    /// whitespace or a comment, one over a comment or over spaces before a line's end, one to
    /// the file's end, one past it.
    #[test]
    fn a_span_the_code_text_does_not_keep_exactly_has_no_places() {
        let noted = "fn f() { // opens\n    let x = 1;  \n    /// doc\n    g(x);\n}\n";
        let len = noted.len() as u32;
        let (comment, comment_end) = at(noted, "// opens");
        let (brace, _) = at(noted, "{");
        let (_, call_end) = at(noted, "g(x);");
        let (let_lo, let_hi) = at(noted, "let x = 1;");
        for (span, what) in [
            ((comment, comment_end), "a comment"),
            ((comment + 3, comment + 5), "inside a comment"),
            ((brace, call_end), "over a comment"),
            ((let_lo, let_hi + 1), "into the spaces before a line's end"),
            ((let_lo, call_end), "over spaces before a line's end and a doc comment"),
            ((0, len), "to the file's end, after its last token"),
            ((0, len + 1), "past the file's end"),
            ((let_hi, let_lo), "backwards"),
        ] {
            assert_eq!(places(noted, Edition::CURRENT, &[span]), None, "{what}");
        }
        assert!(places(noted, Edition::CURRENT, &[(let_lo, let_hi)]).is_some());
    }

    /// A place a text does not have has no offset: a line past its last, a column past its
    /// line's end or inside a character.
    #[test]
    fn a_place_a_text_lacks_has_no_offset() {
        assert_eq!(offset_of("ab\ncd\n", Place { line: 1, column: 2 }), Some(5));
        assert_eq!(offset_of("ab\ncd\n", Place { line: 1, column: 3 }), None);
        assert_eq!(offset_of("ab\ncd\n", Place { line: 3, column: 0 }), None);
        assert_eq!(offset_of("ab\ncd", Place { line: 1, column: 2 }), Some(5));
        assert_eq!(offset_of("é", Place { line: 0, column: 1 }), None);
        assert_eq!(place_of("ab\ncd", 4), Some(Place { line: 1, column: 1 }));
        assert_eq!(place_of("é", 1), None);
        assert_eq!(place_of("ab", 3), None);
    }
}
