//! A cursor over the tokens of a single statement.
//!
//! Statements are tokenized in full before parsing, so operand parsers (which
//! are architecture- and syntax-specific) can be handed plain slices and can
//! backtrack freely.

use crate::lexer::{Punct, TokKind, Token};
use crate::source::Span;

#[derive(Clone)]
pub struct Cursor<'a> {
    toks: &'a [Token],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(toks: &'a [Token]) -> Cursor<'a> {
        Cursor { toks, pos: 0 }
    }

    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn set_pos(&mut self, pos: usize) {
        self.pos = pos;
    }

    pub fn rest(&self) -> &'a [Token] {
        &self.toks[self.pos.min(self.toks.len())..]
    }

    pub fn all(&self) -> &'a [Token] {
        self.toks
    }

    /// The token at the cursor. Past the end this yields a synthetic `Eof`
    /// positioned at the end of the statement, so error spans stay sensible.
    pub fn peek(&self) -> Token {
        self.nth(0)
    }

    pub fn nth(&self, n: usize) -> Token {
        match self.toks.get(self.pos + n) {
            Some(t) => *t,
            None => Token {
                kind: TokKind::Eof,
                span: self.end_span(),
                preceded_by_space: true,
            },
        }
    }

    fn end_span(&self) -> Span {
        match self.toks.last() {
            Some(t) => Span::new(t.span.hi, t.span.hi),
            None => Span::DUMMY,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pos >= self.toks.len()
    }

    /// True when nothing but end-of-statement remains.
    pub fn at_end(&self) -> bool {
        matches!(self.peek().kind, TokKind::Eof | TokKind::Eol)
    }

    pub fn advance(&mut self) -> Token {
        let t = self.peek();
        if self.pos < self.toks.len() {
            self.pos += 1;
        }
        t
    }

    /// Consumes and returns the next token if it is `p`.
    pub fn eat_punct(&mut self, p: Punct) -> Option<Token> {
        if self.peek().is_punct(p) {
            Some(self.advance())
        } else {
            None
        }
    }

    pub fn check_punct(&self, p: Punct) -> bool {
        self.peek().is_punct(p)
    }

    /// Span covering everything from the cursor to the end of the statement.
    pub fn remaining_span(&self) -> Span {
        let rest = self.rest();
        match (rest.first(), rest.last()) {
            (Some(a), Some(b)) => a.span.to(b.span),
            _ => self.end_span(),
        }
    }

    /// Splits the remaining tokens on top-level commas, ignoring commas nested
    /// inside `()`, `[]` or `{}`. Used to break an operand list apart before
    /// handing each piece to an architecture's operand parser.
    pub fn split_commas(&self) -> Vec<&'a [Token]> {
        let rest = self.rest();
        let mut out = Vec::new();
        let mut depth = 0i32;
        let mut start = 0usize;
        let mut saw_any = false;
        for (i, t) in rest.iter().enumerate() {
            match t.kind {
                TokKind::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                TokKind::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => depth -= 1,
                TokKind::Punct(Punct::Comma) if depth <= 0 => {
                    out.push(&rest[start..i]);
                    start = i + 1;
                    saw_any = true;
                    continue;
                }
                _ => {}
            }
            saw_any = true;
        }
        if saw_any || start < rest.len() {
            out.push(&rest[start..]);
        }
        out
    }
}
