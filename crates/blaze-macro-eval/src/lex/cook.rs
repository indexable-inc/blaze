//! rustc's lexer proper (`rustc_parse::lexer::Lexer::next_token_from_cursor` and the `cook_*`
//! functions): `rustc_lexer`'s tokens made into rustc's, with every error rustc emits on the
//! way. A fatal error (an unterminated literal or block comment, a bad raw string) stops the
//! lexer as rustc's `FatalError` does.

use parser::cook::{self, doc_comment_errors, escaped_char};
use rustc_lexer::{Cursor, DocStyle, FrontmatterAllowed, LiteralKind};
use span::Edition;
use unicode_normalization::{IsNormalized, UnicodeNormalization, is_nfc_quick};

use intern::Symbol;

use super::unicode::{confusable, is_confusable};
use super::{Emitted, Lint};
use crate::print::Op;
use crate::stream::{Delimiter, DocKind, IdentKind, LitKind};

/// rustc's `FatalError`: the error was pushed to [`Emitted::errors`].
#[derive(Debug)]
pub(super) struct Fatal;

/// One of rustc's tokens (`rustc_ast::token::TokenKind`, the kinds its lexer makes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Op(Op),
    Open(Delimiter),
    Close(Delimiter),
    Ident(Symbol, IdentKind),
    /// With its leading `'`.
    Lifetime(Symbol, IdentKind),
    Literal { kind: LitKind, symbol: Symbol, suffix: Option<Symbol> },
    Doc { kind: DocKind, inner: bool, data: Symbol },
    Eof,
}

/// A token and its bytes `lo..hi` of the text.
#[derive(Clone, Copy, Debug)]
pub(super) struct Token {
    pub(super) kind: Kind,
    pub(super) lo: u32,
    pub(super) hi: u32,
}

impl Token {
    /// `Token::glue`: of rustc's tokens only punctuation glues (its lexer never makes the lone
    /// `'` that glues to an identifier).
    pub(super) fn glue(&self, next: &Token) -> Option<Token> {
        match (self.kind, next.kind) {
            (Kind::Op(a), Kind::Op(b)) => {
                a.glue(b).map(|op| Token { kind: Kind::Op(op), lo: self.lo, hi: next.hi })
            }
            _ => None,
        }
    }

    pub(super) fn is_punct(&self) -> bool {
        matches!(self.kind, Kind::Op(_))
    }
}

/// `nfc_normalize`.
pub(super) fn nfc_normalize(string: &str) -> Symbol {
    match is_nfc_quick(string.chars()) {
        IsNormalized::Yes => Symbol::intern(string),
        // alloc: text that is not in NFC, rare.
        _ => Symbol::intern(&string.chars().nfc().collect::<String>()),
    }
}

/// `Symbol::can_be_raw`.
pub(super) fn can_be_raw(sym: &str) -> bool {
    !matches!(sym, "" | "_" | "super" | "self" | "Self" | "crate" | "{{root}}" | "$crate")
}

/// `contains_text_flow_control_chars`.
fn contains_text_flow_control_chars(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut at = 0;
    while let Some(idx) = bytes[at..].iter().position(|&b| b == 0xE2) {
        let i = at + idx;
        match bytes[i..i + 3] {
            [_, 0x80, 0xAA..=0xAE] | [_, 0x81, 0xA6..=0xA9] => return true,
            _ => at = i + 3,
        }
    }
    false
}

pub(super) struct Lexer<'s, 'e> {
    /// The text, normalized as rustc's `SourceFile` is.
    src: &'s str,
    cursor: Cursor<'s>,
    /// Where `cursor` is in `src`.
    pos: u32,
    /// The edition of the tokens' span.
    edition: Edition,
    /// An unknown `\u{a0}` was reported already, so the next ones are whitespace.
    nbsp_is_whitespace: bool,
    pub(super) emitted: &'e mut Emitted,
}

impl<'s, 'e> Lexer<'s, 'e> {
    pub(super) fn new(src: &'s str, edition: Edition, emitted: &'e mut Emitted) -> Self {
        Lexer {
            src,
            cursor: Cursor::new(src, FrontmatterAllowed::No),
            pos: 0,
            edition,
            nbsp_is_whitespace: false,
            emitted,
        }
    }

    fn str_from(&self, start: u32) -> &'s str {
        &self.src[start as usize..self.pos as usize]
    }

    fn str_from_to(&self, start: u32, end: u32) -> &'s str {
        &self.src[start as usize..end as usize]
    }

    fn error(&mut self, message: impl Into<String>) {
        // alloc: an error's message, said once
        self.emitted.errors.push(message.into());
    }

    fn fatal(&mut self, message: impl Into<String>) -> Fatal {
        // alloc: as for `error`
        self.emitted.errors.push(message.into());
        Fatal
    }

    /// Lexes again from `at`, as rustc does when it takes back part of a token.
    fn reset_to(&mut self, at: u32) {
        self.pos = at;
        self.cursor = Cursor::new(&self.src[at as usize..], FrontmatterAllowed::No);
    }

    /// The next token, and whether whitespace or a comment came before it.
    pub(super) fn next_token(&mut self) -> Result<(Token, bool), Fatal> {
        let mut preceded_by_whitespace = false;
        let mut swallow_next_invalid = 0usize;
        loop {
            let token = self.cursor.advance_token();
            let start = self.pos;
            self.pos += token.len;

            let kind = match token.kind {
                rustc_lexer::TokenKind::LineComment { doc_style } => {
                    let Some(doc_style) = doc_style else {
                        self.lint_unicode_text_flow(start);
                        preceded_by_whitespace = true;
                        continue;
                    };
                    let content = self.str_from(start + 3);
                    self.lint_doc_comment_unicode_text_flow(content);
                    self.cook_doc_comment(content, DocKind::Line, doc_style)
                }
                rustc_lexer::TokenKind::BlockComment { doc_style, terminated } => {
                    if !terminated {
                        return Err(self.fatal(
                            match doc_style {
                                Some(_) => "unterminated block doc-comment",
                                None => "unterminated block comment",
                            },
                        ));
                    }
                    let Some(doc_style) = doc_style else {
                        self.lint_unicode_text_flow(start);
                        preceded_by_whitespace = true;
                        continue;
                    };
                    let content = self.str_from_to(start + 3, self.pos - 2);
                    self.lint_doc_comment_unicode_text_flow(content);
                    self.cook_doc_comment(content, DocKind::Block, doc_style)
                }
                // `FrontmatterAllowed::No` makes none; rustc skips one as whitespace.
                rustc_lexer::TokenKind::Frontmatter { .. }
                | rustc_lexer::TokenKind::Whitespace => {
                    preceded_by_whitespace = true;
                    continue;
                }
                rustc_lexer::TokenKind::Ident => self.ident(start),
                rustc_lexer::TokenKind::RawIdent => {
                    let sym = nfc_normalize(self.str_from(start + 2));
                    if !can_be_raw(sym.as_str()) {
                        self.error(format!("`{}` cannot be a raw identifier", sym.as_str())); // alloc: error path
                    }
                    Kind::Ident(sym, IdentKind::Raw)
                }
                rustc_lexer::TokenKind::ForcedKeywordIdent if self.edition >= Edition::Edition2021 => {
                    self.emitted.forced_keywords += 1;
                    Kind::Ident(nfc_normalize(self.str_from(start + 2)), IdentKind::ForcedKeyword)
                }
                // Only the `k` is an identifier before 2021.
                rustc_lexer::TokenKind::ForcedKeywordIdent => {
                    self.reset_to(start + 1);
                    self.ident(start)
                }
                rustc_lexer::TokenKind::UnknownPrefix => {
                    self.report_unknown_prefix(start);
                    self.ident(start)
                }
                rustc_lexer::TokenKind::UnknownPrefixLifetime => {
                    self.report_unknown_prefix(start);
                    Kind::Lifetime(Symbol::intern(self.str_from(start)), IdentKind::Normal)
                }
                rustc_lexer::TokenKind::InvalidIdent if !self.is_one_confusable(start) => {
                    let sym = nfc_normalize(self.str_from(start));
                    // Reported once per identifier after expansion (`bad_unicode_identifiers`).
                    let message = match sym.as_str() {
                        "🦀" => "Ferris cannot be used as an identifier".to_owned(), // alloc: error path
                        text => format!("identifiers cannot contain emoji: `{text}`"), // alloc: error path
                    };
                    if !self.emitted.errors.contains(&message) {
                        self.error(message);
                    }
                    Kind::Ident(sym, IdentKind::Normal)
                }
                rustc_lexer::TokenKind::Literal {
                    kind: kind @ (LiteralKind::CStr { .. } | LiteralKind::RawCStr { .. }),
                    ..
                } if self.edition < Edition::Edition2021 => {
                    // Only the prefix (`c` or `cr`) is an identifier before 2021.
                    let prefix_len = if matches!(kind, LiteralKind::CStr { .. }) { 1 } else { 2 };
                    self.reset_to(start + prefix_len);
                    self.ident(start)
                }
                rustc_lexer::TokenKind::GuardedStrPrefix => self.maybe_report_guarded_str(start)?,
                rustc_lexer::TokenKind::Literal { kind, suffix_start } => {
                    let suffix_start = start + suffix_start;
                    let (kind, symbol) = self.cook_lexer_literal(start, suffix_start, kind)?;
                    let suffix = if suffix_start < self.pos {
                        let string = self.str_from(suffix_start);
                        if string == "_" {
                            self.error("underscore literal suffix is not allowed");
                            None
                        } else {
                            Some(Symbol::intern(string))
                        }
                    } else {
                        None
                    };
                    self.lint_literal_unicode_text_flow(symbol, kind);
                    Kind::Literal { kind, symbol, suffix }
                }
                rustc_lexer::TokenKind::Lifetime { starts_with_number } => {
                    let lifetime_name = nfc_normalize(self.str_from(start));
                    if starts_with_number {
                        // Stashed by rustc's lexer, and emitted unless the parser replaces it.
                        self.error("lifetimes cannot start with a number");
                    }
                    Kind::Lifetime(lifetime_name, IdentKind::Normal)
                }
                rustc_lexer::TokenKind::RawLifetime => {
                    let ident_start = start + 3;
                    if self.edition >= Edition::Edition2021 {
                        if self.cursor.as_str().starts_with('\'') {
                            self.error(
                                "character literal may only contain one codepoint",
                            );
                        }
                        let name = self.str_from(ident_start);
                        if !can_be_raw(name) {
                            self.error(format!("`{name}` cannot be a raw lifetime")); // alloc: error path
                        }
                        // alloc: the `'` put back before the name, as rustc does.
                        let sym = nfc_normalize(&format!("'{name}"));
                        Kind::Lifetime(sym, IdentKind::Raw)
                    } else {
                        // Only `'r` is a lifetime before 2021.
                        self.reset_to(start + 2);
                        Kind::Lifetime(nfc_normalize(self.str_from(start)), IdentKind::Normal)
                    }
                }
                rustc_lexer::TokenKind::Semi => Kind::Op(Op::Semi),
                rustc_lexer::TokenKind::Comma => Kind::Op(Op::Comma),
                rustc_lexer::TokenKind::Dot => Kind::Op(Op::Dot),
                rustc_lexer::TokenKind::OpenParen => Kind::Open(Delimiter::Parenthesis),
                rustc_lexer::TokenKind::CloseParen => Kind::Close(Delimiter::Parenthesis),
                rustc_lexer::TokenKind::OpenBrace => Kind::Open(Delimiter::Brace),
                rustc_lexer::TokenKind::CloseBrace => Kind::Close(Delimiter::Brace),
                rustc_lexer::TokenKind::OpenBracket => Kind::Open(Delimiter::Bracket),
                rustc_lexer::TokenKind::CloseBracket => Kind::Close(Delimiter::Bracket),
                rustc_lexer::TokenKind::At => Kind::Op(Op::At),
                rustc_lexer::TokenKind::Pound => Kind::Op(Op::Pound),
                rustc_lexer::TokenKind::Tilde => Kind::Op(Op::Tilde),
                rustc_lexer::TokenKind::Question => Kind::Op(Op::Question),
                rustc_lexer::TokenKind::Colon => Kind::Op(Op::Colon),
                rustc_lexer::TokenKind::Dollar => Kind::Op(Op::Dollar),
                rustc_lexer::TokenKind::Eq => Kind::Op(Op::Eq),
                rustc_lexer::TokenKind::Bang => Kind::Op(Op::Bang),
                rustc_lexer::TokenKind::Lt => Kind::Op(Op::Lt),
                rustc_lexer::TokenKind::Gt => Kind::Op(Op::Gt),
                rustc_lexer::TokenKind::Minus => Kind::Op(Op::Minus),
                rustc_lexer::TokenKind::And => Kind::Op(Op::And),
                rustc_lexer::TokenKind::Or => Kind::Op(Op::Or),
                rustc_lexer::TokenKind::Plus => Kind::Op(Op::Plus),
                rustc_lexer::TokenKind::Star => Kind::Op(Op::Star),
                rustc_lexer::TokenKind::Slash => Kind::Op(Op::Slash),
                rustc_lexer::TokenKind::Caret => Kind::Op(Op::Caret),
                rustc_lexer::TokenKind::Percent => Kind::Op(Op::Percent),
                rustc_lexer::TokenKind::Unknown | rustc_lexer::TokenKind::InvalidIdent => {
                    // No error for a run of the same invalid token.
                    if swallow_next_invalid > 0 {
                        swallow_next_invalid -= 1;
                        continue;
                    }
                    let mut it = self.src[start as usize..].chars();
                    let c = it.next().expect("an unknown token has a character");
                    if c == '\u{a0}' {
                        if self.nbsp_is_whitespace {
                            preceded_by_whitespace = true;
                            continue;
                        }
                        self.nbsp_is_whitespace = true;
                    }
                    let repeats = it.take_while(|c1| *c1 == c).count();
                    let token = substitution(c);
                    let mut message = String::from("unknown start of token: "); // alloc: error path
                    escaped_char(c, &mut message);
                    self.error(message);
                    if repeats > 0 {
                        swallow_next_invalid = repeats;
                    }
                    match token {
                        Some(kind) => kind,
                        None => {
                            preceded_by_whitespace = true;
                            continue;
                        }
                    }
                }
                rustc_lexer::TokenKind::Eof => Kind::Eof,
            };
            return Ok((Token { kind, lo: start, hi: self.pos }, preceded_by_whitespace));
        }
    }

    fn ident(&self, start: u32) -> Kind {
        Kind::Ident(nfc_normalize(self.str_from(start)), IdentKind::Normal)
    }

    /// The token is the one character rustc's confusables table has.
    fn is_one_confusable(&self, start: u32) -> bool {
        let mut chars = self.str_from(start).chars();
        matches!((chars.next(), chars.next()), (Some(c), None) if is_confusable(c))
    }

    /// `report_unknown_prefix`: an error from 2021, a lint (allowed by default) before.
    fn report_unknown_prefix(&mut self, start: u32) {
        if self.edition >= Edition::Edition2021 {
            let prefix = self.str_from(start);
            self.error(format!("prefix `{prefix}` is unknown")); // alloc: error path
        }
    }

    /// `maybe_report_guarded_str`: `#"..."` or `##` is reserved from 2024; before, it is a
    /// `#` and what follows.
    fn maybe_report_guarded_str(&mut self, start: u32) -> Result<Kind, Fatal> {
        let edition2024 = self.edition >= Edition::Edition2024;
        let mut cursor =
            Cursor::new(&self.src[start as usize..], FrontmatterAllowed::No);
        let (is_string, unterminated) = match cursor.guarded_double_quoted_string() {
            Some(rustc_lexer::GuardedStr { n_hashes: _, terminated, token_len }) => {
                if edition2024 {
                    self.cursor = cursor;
                    self.pos = start + token_len;
                }
                (true, !terminated)
            }
            None => (false, false),
        };
        if edition2024 {
            if unterminated {
                return Err(self.fatal("unterminated double quote string"));
            }
            self.error(
                match is_string {
                    true => "invalid string literal",
                    false => "reserved multi-hash token is forbidden",
                },
            );
            Ok(Kind::Literal {
                kind: LitKind::Err,
                symbol: Symbol::intern(self.str_from(start)),
                suffix: None,
            })
        } else {
            self.reset_to(start + 1);
            Ok(Kind::Op(Op::Pound))
        }
    }

    fn cook_doc_comment(&mut self, content: &str, kind: DocKind, doc_style: DocStyle) -> Kind {
        let errors = &mut self.emitted.errors;
        doc_comment_errors(content, kind == DocKind::Block, |e| errors.push(e.message.into_owned()));
        Kind::Doc {
            kind,
            inner: doc_style == DocStyle::Inner,
            data: Symbol::intern(content),
        }
    }

    /// `cook_lexer_literal`, a fatal error stopping the lexer.
    fn cook_lexer_literal(
        &mut self,
        start: u32,
        end: u32,
        kind: LiteralKind,
    ) -> Result<(LitKind, Symbol), Fatal> {
        let src: &'s str = self.src;
        let rest = &src[start as usize..];
        let errors = &mut self.emitted.errors;
        let cooked = cook::cook_lexer_literal(rest, self.pos - start, end - start, kind, |e| {
            errors.push(e.message.into_owned());
        });
        match cooked {
            Ok((kind, symbol)) => Ok((kind, Symbol::intern(symbol))),
            Err(fatal) => Err(self.fatal(fatal.message.into_owned())),
        }
    }

    fn lint_unicode_text_flow(&mut self, start: u32) {
        if contains_text_flow_control_chars(self.str_from(start + 2)) {
            self.emitted.lints.push((
                Lint::TextDirectionCodepointInComment,
                "unicode codepoint changing visible direction of text present in comment".to_owned(), // alloc: error path
            ));
        }
    }

    fn lint_doc_comment_unicode_text_flow(&mut self, content: &str) {
        if contains_text_flow_control_chars(content) {
            self.text_direction_in("doc comment");
        }
    }

    fn lint_literal_unicode_text_flow(&mut self, text: Symbol, kind: LitKind) {
        if kind != LitKind::Err && contains_text_flow_control_chars(text.as_str()) {
            self.text_direction_in("literal");
        }
    }

    fn text_direction_in(&mut self, label: &str) {
        self.emitted.lints.push((
            Lint::TextDirectionCodepointInLiteral,
            format!("unicode codepoint changing visible direction of text present in {label}"), // alloc: error path
        ));
    }
}

/// `check_for_substitution`'s token: what rustc lexes a confusable as, with its error.
fn substitution(c: char) -> Option<Kind> {
    Some(match confusable(c)? {
        "_" => Kind::Ident(Symbol::intern("_"), IdentKind::Normal),
        "-" => Kind::Op(Op::Minus),
        "," => Kind::Op(Op::Comma),
        ";" => Kind::Op(Op::Semi),
        ":" => Kind::Op(Op::Colon),
        "!" => Kind::Op(Op::Bang),
        "?" => Kind::Op(Op::Question),
        "." => Kind::Op(Op::Dot),
        "(" => Kind::Open(Delimiter::Parenthesis),
        ")" => Kind::Close(Delimiter::Parenthesis),
        "[" => Kind::Open(Delimiter::Bracket),
        "]" => Kind::Close(Delimiter::Bracket),
        "{" => Kind::Open(Delimiter::Brace),
        "}" => Kind::Close(Delimiter::Brace),
        "*" => Kind::Op(Op::Star),
        "/" => Kind::Op(Op::Slash),
        "&" => Kind::Op(Op::And),
        "+" => Kind::Op(Op::Plus),
        "<" => Kind::Op(Op::Lt),
        "=" => Kind::Op(Op::Eq),
        "==" => Kind::Op(Op::EqEq),
        ">" => Kind::Op(Op::Gt),
        // Space, backslash and the quotes have no token.
        _ => return None,
    })
}
