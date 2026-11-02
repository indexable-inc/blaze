//! rustc's lexer loop over one file (`rustc_parse/src/lexer/mod.rs`, `next_token_from_cursor`),
//! as far as the lints need it: what it buffers of comments, literals, reserved prefixes and
//! guarded strings, and the identifiers it puts in the symbol gallery. It runs on the file's
//! text as rustc's source map has it (`rustc_span/src/lib.rs`, `normalize_src`: no byte order
//! mark, `\r\n` as `\n`), with the pinned nightly's lexer (`crates/rustc_lexer`), and gives ranges in
//! the file's own text.
//!
//! Follows `compiler/rustc_parse/src/lexer/mod.rs` (`lex_token_trees`, `next_token_from_cursor`,
//! `lint_unicode_text_flow`, `lint_doc_comment_unicode_text_flow`,
//! `lint_literal_unicode_text_flow`, `report_text_direction_codepoint`, `cook_lexer_literal`,
//! `cook_quoted`, `report_unknown_prefix`, `maybe_report_guarded_str`, `nfc_normalize`),
//! `compiler/rustc_ast/src/util/unicode.rs` (`TEXT_FLOW_CONTROL_CHARS`) and
//! `compiler/rustc_span/src/lib.rs` (`remove_bom`, `normalize_newlines`) (MIT or Apache-2.0).

use std::borrow::Cow;

use rustc_lexer::{Cursor, FrontmatterAllowed, LiteralKind, TokenKind};
use span::Edition;
use syntax::{
    TextRange, TextSize,
    unescape::{Mode, check_for_errors},
};
use unicode_normalization::UnicodeNormalization as _;

/// The codepoints that change the direction text is shown in (`TEXT_FLOW_CONTROL_CHARS`).
pub(super) const TEXT_FLOW_CONTROL_CHARS: &[char] = &[
    '\u{202A}', '\u{202B}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{202C}',
    '\u{2069}',
];

/// What rustc's lexer buffers of a file, in the order it buffers it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Lexed {
    /// `text_direction_codepoint_in_comment` of a comment that is not a doc comment
    /// (`UnicodeTextFlow`): the comment, and each codepoint in it.
    Comment { range: TextRange, chars: Vec<(char, TextRange)> },
    /// `text_direction_codepoint_in_literal` (`HiddenUnicodeCodepointsDiag`) of a literal or a
    /// doc comment: what `label` names it ("literal", "doc comment"), each codepoint, whether
    /// the codepoints are pointed at and escaped (`point_at_inner_spans`), and whether it is a
    /// doc comment.
    Literal {
        range: TextRange,
        label: &'static str,
        chars: Vec<(char, TextRange)>,
        inner: bool,
        doc: bool,
    },
    /// `rust_2021_prefixes_incompatible_syntax` of an unknown prefix before 2021
    /// (`ReservedPrefix`): the prefix, `'` included for a lifetime's.
    UnknownPrefix { range: TextRange },
    /// `rust_2021_prefixes_incompatible_syntax` of `'r#` before 2021 (`RawPrefix`): the `'r#`.
    RawPrefix { range: TextRange },
    /// `rust_2024_guarded_string_incompatible_syntax` before 2024: a guarded string
    /// (`ReservedStringLint`) when `string`, else `##` (`ReservedMultihashLint`), and where
    /// whitespace would avoid it.
    Guarded { range: TextRange, string: bool, space: TextSize },
}

/// An identifier rustc's lexer puts in the symbol gallery: the token, and whether it is raw
/// (its symbol is what follows the `r#`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ident {
    pub(super) range: TextRange,
    pub(super) raw: bool,
}

impl Ident {
    /// Where its symbol is written: the token without its `r#`.
    pub(super) fn symbol_range(self) -> TextRange {
        match self.raw {
            true => TextRange::new(self.range.start() + TextSize::from(2), self.range.end()),
            false => self.range,
        }
    }
}

/// What rustc's lexer gives of a file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LexedFile {
    pub(super) lexed: Vec<Lexed>,
    /// Each identifier, in the order lexed.
    pub(super) idents: Vec<Ident>,
}

/// rustc's `nfc_normalize`: the symbol interned for an identifier's text.
pub(super) fn nfc(ident: &str) -> Cow<'_, str> {
    match ident.is_ascii() {
        true => Cow::Borrowed(ident),
        // alloc: a non-ASCII identifier only, normalized as rustc interns it
        false => Cow::Owned(ident.nfc().collect()),
    }
}

/// Whether `text` holds a codepoint of [`TEXT_FLOW_CONTROL_CHARS`] (`contains_text_flow_control_chars`).
pub(super) fn has_text_flow_control(text: &str) -> bool {
    // Each codepoint is three bytes starting with 0xE2 in UTF-8.
    memchr::memchr(0xE2, text.as_bytes()).is_some() && text.chars().any(|c| TEXT_FLOW_CONTROL_CHARS.contains(&c))
}

/// A file's text as rustc's source map holds it, and how to find a position of it in the
/// file's own text.
struct Normalized<'a> {
    text: Cow<'a, str>,
    /// The length of the byte order mark removed from the start.
    bom: u32,
    /// For each `\r` removed before a `\n`, where that `\n` is in [`Normalized::text`]: a
    /// position after it is one byte further in the file's own text.
    removed: Vec<u32>,
}

impl<'a> Normalized<'a> {
    fn of(text: &'a str) -> Normalized<'a> {
        let (bom, rest) = match text.strip_prefix('\u{feff}') {
            Some(rest) => ('\u{feff}'.len_utf8() as u32, rest),
            None => (0, text),
        };
        if memchr::memmem::find(rest.as_bytes(), b"\r\n").is_none() {
            // alloc: empty, allocates nothing
            return Normalized { text: Cow::Borrowed(rest), bom, removed: Vec::new() };
        }
        let mut normalized = String::with_capacity(rest.len());
        // alloc: once per file with a `\r\n`, as rustc's source map normalizes it
        let mut removed = Vec::new();
        let mut copied = 0;
        for (at, _) in rest.match_indices("\r\n") {
            normalized.push_str(&rest[copied..at]);
            removed.push(normalized.len() as u32);
            copied = at + 1;
        }
        normalized.push_str(&rest[copied..]);
        Normalized { text: Cow::Owned(normalized), bom, removed }
    }

    /// Where `pos` of [`Normalized::text`] is in the file's own text (rustc's
    /// `NormalizedPos`: a removed `\r` counts from the byte after its `\n`).
    fn original(&self, pos: u32) -> TextSize {
        let before = self.removed.partition_point(|&it| it < pos) as u32;
        TextSize::from(pos + self.bom + before)
    }
}

#[cfg(test)]
fn lex(text: &str, edition: Edition) -> LexedFile {
    let mut out = LexedFile::default();
    lex_into(text, edition, &mut out);
    out
}

/// What rustc's lexer buffers of `text`, a file of a crate of `edition`, and its identifiers,
/// into `out`, cleared first: one buffer serves every file of the crate. It stops where rustc's
/// lexer stops with a fatal error (an unterminated literal or block comment, a bad raw string):
/// rustc gives nothing after that.
pub(super) fn lex_into(text: &str, edition: Edition, out: &mut LexedFile) {
    out.lexed.clear();
    out.idents.clear();
    let normalized = Normalized::of(text);
    let src: &str = &normalized.text;
    let start = rustc_lexer::strip_shebang(src).unwrap_or(0) as u32;
    let mut lexer = Lexer {
        src,
        normalized: &normalized,
        pos: start,
        cursor: Cursor::new(&src[start as usize..], FrontmatterAllowed::Yes),
        edition,
        out,
    };
    lexer.run();
}

struct Lexer<'a, 'o> {
    src: &'a str,
    normalized: &'a Normalized<'a>,
    /// Where the cursor is in `src`.
    pos: u32,
    cursor: Cursor<'a>,
    edition: Edition,
    out: &'o mut LexedFile,
}

/// Whether rustc's lexer goes on after a token.
enum Next {
    Go,
    Fatal,
}

impl<'a> Lexer<'a, '_> {
    fn run(&mut self) {
        loop {
            let str_before = self.cursor.as_str();
            let token = self.cursor.advance_token();
            let start = self.pos;
            self.pos += token.len;
            let next = match token.kind {
                TokenKind::LineComment { doc_style: None } => self.comment(start),
                TokenKind::LineComment { doc_style: Some(_) } => self.doc_comment(start, start + 3, self.pos),
                TokenKind::BlockComment { terminated: false, .. } => Next::Fatal,
                TokenKind::BlockComment { doc_style: None, .. } => self.comment(start),
                TokenKind::BlockComment { doc_style: Some(_), .. } => {
                    self.doc_comment(start, start + 3, self.pos - 2)
                }
                TokenKind::Ident => self.ident(start, false),
                TokenKind::RawIdent => self.ident(start, true),
                TokenKind::UnknownPrefix => {
                    self.unknown_prefix(start);
                    self.ident(start, false)
                }
                TokenKind::ForcedKeywordIdent if self.edition.at_least_2021() => self.ident(start + 2, false),
                TokenKind::ForcedKeywordIdent => {
                    // Split into the prefix `k`, an identifier, and `#` and the rest after it.
                    self.pos = start + 1;
                    self.cursor = Cursor::new(&str_before[1..], FrontmatterAllowed::No);
                    self.unknown_prefix(start);
                    self.ident(start, false)
                }
                TokenKind::UnknownPrefixLifetime => self.unknown_prefix(start),
                TokenKind::Literal { kind: kind @ (LiteralKind::CStr { .. } | LiteralKind::RawCStr { .. }), .. }
                    if !self.edition.at_least_2021() =>
                {
                    // Split into the prefix, an identifier, and the string literal after it.
                    let prefix_len: u32 = match kind {
                        LiteralKind::CStr { .. } => 1,
                        _ => 2,
                    };
                    self.pos = start + prefix_len;
                    self.cursor = Cursor::new(&str_before[prefix_len as usize..], FrontmatterAllowed::No);
                    self.unknown_prefix(start);
                    self.ident(start, false)
                }
                TokenKind::GuardedStrPrefix => self.guarded_str(start, str_before),
                TokenKind::Literal { kind, suffix_start } => self.literal(start, start + suffix_start, kind),
                TokenKind::RawLifetime if !self.edition.at_least_2021() => {
                    // Lexed again as the lifetime `'r`, then `#` and the rest.
                    let range = self.range(start, start + 3);
                    self.out.lexed.push(Lexed::RawPrefix { range });
                    self.pos = start + 2;
                    self.cursor = Cursor::new(&str_before[2..], FrontmatterAllowed::No);
                    Next::Go
                }
                TokenKind::Eof => return,
                _ => Next::Go,
            };
            if let Next::Fatal = next {
                return;
            }
        }
    }

    fn range(&self, lo: u32, hi: u32) -> TextRange {
        TextRange::new(self.normalized.original(lo), self.normalized.original(hi))
    }

    fn text(&self, lo: u32, hi: u32) -> &'a str {
        &self.src[lo as usize..hi as usize]
    }

    /// Each codepoint of [`TEXT_FLOW_CONTROL_CHARS`] in `text`, which is at `at` of the
    /// source.
    fn flow_chars(&self, text: &str, at: u32) -> Vec<(char, TextRange)> {
        text.char_indices()
            .filter(|(_, c)| TEXT_FLOW_CONTROL_CHARS.contains(c))
            .map(|(i, c)| {
                let lo = at + i as u32;
                (c, self.range(lo, lo + c.len_utf8() as u32))
            })
            // alloc: only once `has_text_flow_control` found one, for the lint's diagnostic
            .collect()
    }

    /// rustc's `ident` (and its `RawIdent`): the token, in the gallery.
    fn ident(&mut self, start: u32, raw: bool) -> Next {
        let range = self.range(start, self.pos);
        self.out.idents.push(Ident { range, raw });
        Next::Go
    }

    /// `lint_unicode_text_flow`: a comment's text after its `//` or `/*`.
    fn comment(&mut self, start: u32) -> Next {
        let content = self.text(start + 2, self.pos);
        if has_text_flow_control(content) {
            let chars = self.flow_chars(content, start + 2);
            let range = self.range(start, self.pos);
            self.out.lexed.push(Lexed::Comment { range, chars });
        }
        Next::Go
    }

    /// `lint_doc_comment_unicode_text_flow`: a doc comment's text between its delimiters.
    /// rustc places each codepoint from the comment's start, not its text's
    /// (`report_text_direction_codepoint` with no padding); those places are used for nothing
    /// but the escaped codepoints' list.
    fn doc_comment(&mut self, start: u32, content_lo: u32, content_hi: u32) -> Next {
        let content = self.text(content_lo, content_hi);
        if has_text_flow_control(content) {
            let chars = self.flow_chars(content, start);
            let range = self.range(start, self.pos);
            self.out.lexed.push(Lexed::Literal { range, label: "doc comment", chars, inner: false, doc: true });
        }
        Next::Go
    }

    /// `report_unknown_prefix`, before 2021: the prefix up to where the lexer is.
    fn unknown_prefix(&mut self, start: u32) -> Next {
        if !self.edition.at_least_2021() {
            let range = self.range(start, self.pos);
            self.out.lexed.push(Lexed::UnknownPrefix { range });
        }
        Next::Go
    }

    /// `maybe_report_guarded_str`: `#"` or `##` at `start`, `str_before` the text from there.
    fn guarded_str(&mut self, start: u32, str_before: &'a str) -> Next {
        let mut cursor = Cursor::new(str_before, FrontmatterAllowed::No);
        let (string, end, terminated) = match cursor.guarded_double_quoted_string() {
            Some(it) => (true, start + it.token_len, it.terminated),
            None => (false, self.pos, true),
        };
        if self.edition.at_least_2024() {
            if string {
                self.cursor = cursor;
                self.pos = end;
            }
            return match terminated {
                true => Next::Go,
                false => Next::Fatal,
            };
        }
        let range = self.range(start, end);
        let space = self.normalized.original(start + 1);
        self.out.lexed.push(Lexed::Guarded { range, string, space });
        self.pos = start + 1;
        self.cursor = Cursor::new(&str_before[1..], FrontmatterAllowed::No);
        Next::Go
    }

    /// `cook_lexer_literal` and `lint_literal_unicode_text_flow`: a literal from `start`, its
    /// suffix from `end`.
    fn literal(&mut self, start: u32, end: u32, kind: LiteralKind) -> Next {
        // The mode its text is unescaped in, the length of what is before and after its text,
        // and whether its codepoints are pointed at (`point_at_inner_spans`).
        let (mode, prefix, postfix, inner) = match kind {
            LiteralKind::Int { .. } | LiteralKind::Float { .. } => return Next::Go,
            LiteralKind::Char { terminated: false }
            | LiteralKind::Byte { terminated: false }
            | LiteralKind::Str { terminated: false }
            | LiteralKind::ByteStr { terminated: false }
            | LiteralKind::CStr { terminated: false }
            | LiteralKind::RawStr { n_hashes: None }
            | LiteralKind::RawByteStr { n_hashes: None }
            | LiteralKind::RawCStr { n_hashes: None } => return Next::Fatal,
            LiteralKind::Char { .. } => (Mode::Char, 1, 1, true),
            LiteralKind::Byte { .. } => (Mode::Byte, 2, 1, false),
            LiteralKind::Str { .. } => (Mode::Str, 1, 1, true),
            LiteralKind::ByteStr { .. } => (Mode::ByteStr, 2, 1, false),
            LiteralKind::CStr { .. } => (Mode::CStr, 2, 1, true),
            LiteralKind::RawStr { n_hashes: Some(n) } => (Mode::RawStr, 2 + u32::from(n), 1 + u32::from(n), true),
            LiteralKind::RawByteStr { n_hashes: Some(n) } => {
                (Mode::RawByteStr, 3 + u32::from(n), 1 + u32::from(n), false)
            }
            LiteralKind::RawCStr { n_hashes: Some(n) } => (Mode::RawCStr, 3 + u32::from(n), 1 + u32::from(n), true),
        };
        let content_start = start + prefix;
        let content = self.text(content_start, end - postfix);
        if !has_text_flow_control(content) {
            return Next::Go;
        }
        let mut failed = false;
        check_for_errors(content, mode, |_, err| failed |= err.is_fatal());
        if failed {
            return Next::Go;
        }
        // rustc places a literal's codepoints from its text's start only when it points at
        // them; else from the literal's start.
        let at = match inner {
            true => content_start,
            false => start,
        };
        let chars = self.flow_chars(content, at);
        let range = self.range(start, self.pos);
        self.out.lexed.push(Lexed::Literal { range, label: "literal", chars, inner, doc: false });
        Next::Go
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(lo: u32, hi: u32) -> TextRange {
        TextRange::new(TextSize::from(lo), TextSize::from(hi))
    }

    fn symbols<'t>(text: &'t str, lexed: &LexedFile) -> Vec<&'t str> {
        lexed.idents.iter().map(|it| &text[it.symbol_range()]).collect()
    }

    #[test]
    fn a_comment_with_a_codepoint_is_buffered_with_where_the_codepoint_is() {
        let text = "// a\u{202e}b\nfn f() {}";
        let lexed = lex(text, Edition::Edition2021);
        assert_eq!(lexed.lexed, [Lexed::Comment { range: at(0, 8), chars: vec![('\u{202e}', at(4, 7))] }]);
        assert_eq!(symbols(text, &lexed), ["fn", "f"]);
    }

    #[test]
    fn an_escaped_codepoint_is_not_one() {
        assert!(lex(r#"const S: &str = "\u{202e}";"#, Edition::Edition2021).lexed.is_empty());
    }

    #[test]
    fn a_literal_points_at_its_codepoints_from_its_text() {
        let lexed = lex("r#\"\u{2066}\"#", Edition::Edition2021);
        let chars = vec![('\u{2066}', at(3, 6))];
        assert_eq!(lexed.lexed, [Lexed::Literal { range: at(0, 8), label: "literal", chars, inner: true, doc: false }]);
    }

    #[test]
    fn a_doc_comment_is_a_literal_that_does_not_point_at_its_codepoints() {
        let lexed = lex("/// \u{2068}x\nfn f() {}", Edition::Edition2024);
        let [Lexed::Literal { range, label, inner, doc, .. }] = lexed.lexed.as_slice() else { panic!("{lexed:?}") };
        assert_eq!((*range, *label, *inner, *doc), (at(0, 8), "doc comment", false, true));
    }

    #[test]
    fn a_literal_with_an_error_is_not_linted() {
        assert!(lex("b\"\u{202e}\"", Edition::Edition2021).lexed.is_empty());
    }

    #[test]
    fn unknown_prefixes_are_lints_before_2021_only() {
        let lexed = lex("m!(z\"hey\" hey#1);", Edition::Edition2018);
        assert_eq!(lexed.lexed, [Lexed::UnknownPrefix { range: at(3, 4) }, Lexed::UnknownPrefix { range: at(10, 13) }]);
        assert!(lex("m!(z\"hey\");", Edition::Edition2021).lexed.is_empty());
    }

    #[test]
    fn a_c_string_before_2021_is_a_prefix_and_a_string() {
        let text = "c\"\u{202e}\"";
        let lexed = lex(text, Edition::Edition2018);
        let chars = vec![('\u{202e}', at(2, 5))];
        assert_eq!(
            lexed.lexed,
            [
                Lexed::UnknownPrefix { range: at(0, 1) },
                Lexed::Literal { range: at(1, 6), label: "literal", chars, inner: true, doc: false },
            ]
        );
        assert_eq!(symbols(text, &lexed), ["c"]);
    }

    #[test]
    fn guarded_strings_are_lexed_again_after_their_first_hash() {
        let lexed = lex("m!(##\"foo\"##);", Edition::Edition2021);
        assert_eq!(
            lexed.lexed,
            [
                Lexed::Guarded { range: at(3, 12), string: true, space: TextSize::from(4) },
                Lexed::Guarded { range: at(4, 11), string: true, space: TextSize::from(5) },
                Lexed::Guarded { range: at(10, 12), string: false, space: TextSize::from(11) },
            ]
        );
        assert!(lex("m!(#\"foo\"#);", Edition::Edition2024).lexed.is_empty());
    }

    #[test]
    fn a_raw_lifetime_before_2021_is_a_prefix() {
        let text = "m!('r#lt);";
        let lexed = lex(text, Edition::Edition2015);
        assert_eq!(lexed.lexed, [Lexed::RawPrefix { range: at(3, 6) }]);
        assert_eq!(symbols(text, &lexed), ["m", "lt"]);
        let text = "fn f<'r#lt>() {}";
        let lexed = lex(text, Edition::Edition2021);
        assert!(lexed.lexed.is_empty());
        assert_eq!(symbols(text, &lexed), ["fn", "f"]);
    }

    #[test]
    fn raw_identifiers_are_symbols_without_their_prefix() {
        let text = "fn r#async() {}";
        let lexed = lex(text, Edition::Edition2018);
        assert_eq!(lexed.idents[1], Ident { range: at(3, 10), raw: true });
        assert_eq!(symbols(text, &lexed), ["fn", "async"]);
    }

    #[test]
    fn identifiers_are_nfc() {
        let text = "fn e\u{301}() {}";
        let lexed = lex(text, Edition::Edition2021);
        assert_eq!(lexed.idents[1].range, at(3, 6));
        assert_eq!(nfc(&text[lexed.idents[1].symbol_range()]), "\u{e9}");
        assert!(matches!(nfc("plain"), Cow::Borrowed("plain")));
    }

    #[test]
    fn places_are_in_the_files_own_text() {
        let lexed = lex("\u{feff}fn f() {}\r\n// \u{202e}\r\n", Edition::Edition2021);
        assert_eq!(lexed.idents[0].range, at(3, 5));
        assert_eq!(lexed.lexed, [Lexed::Comment { range: at(14, 20), chars: vec![('\u{202e}', at(17, 20))] }]);
    }

    #[test]
    fn lexing_stops_at_a_fatal_error() {
        let lexed = lex("/* x\n// \u{202e}", Edition::Edition2021);
        assert!(lexed.lexed.is_empty());
    }

    #[test]
    fn only_the_flow_control_codepoints_count() {
        assert!(has_text_flow_control("a\u{2069}"));
        assert!(!has_text_flow_control("\u{2060}\u{e9} plain"));
    }
}
