//! Where an outline's lexer is among the members of the item it lexes
//! ([`super::LexedStr::new_outline`]): which `{` opens a `fn` body, read from the tokens lexed
//! so far, so the lexer can make the whole body one [`SyntaxKind::LAZY_BODY`] token.
//!
//! A member is an item at brace depth 0, or a member of an `impl` or `trait` at depth 1 (the
//! member level). A member's tokens move it from attributes, doc comments, visibility and
//! qualifiers ([`State::Start`]) to `fn` ([`State::Fn`]), to `impl` or `trait`
//! ([`State::Container`]), or to anything else ([`State::Other`]). A `fn`'s body is the first
//! `{` at member level with no delimiter and no `<` open in its head: angles are counted at
//! member level only, `->` is an arrow and not a close. A head braces in no other way: a const
//! argument or block is inside `[..]`, `(..)` or `<..>` (`[u8; { N + 1 }]`, `Tr<{ N }>`), an
//! attribute's braces are inside `#[..]`. A head this cannot read for certain is refused, its
//! body lexed as any text is ([`State::Other`]): a `<` closed past zero, a contract clause
//! (`requires`, `ensures`, `recommends`, `decreases`: blaze_verify's, which may hold a `<`
//! that no `>` closes), a macro call at member level (`-> m!{..}`), an `=` at member level
//! (`const F: fn() = { .. };` is no `fn` item), and a `{` right after `->`. A `fn` met past a
//! member's start (`impl Tr for fn() -> u8 { .. }`, `type F = fn();`) is no `fn` item.
//!
//! Wrong only costs: a `{` this takes for a body is made a [`SyntaxKind::LAZY_BODY`] only when
//! [`super::scan::body_len`] also finds its close and none of the outline's refusals, and the
//! parser takes that token exactly where it would have taken the `{` (`crate::outline`); a `{`
//! this does not take is lexed as before. A close that does not match its open, or nesting
//! deeper than [`MAX_DEPTH`], stops it: the rest of the text is lexed as before.

use super::LexedStr;
use crate::{
    Edition,
    SyntaxKind::{self, *},
};

/// What the member being lexed is, as far as its tokens so far say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before a member's item keyword: attributes, doc comments, visibility, qualifiers.
    Start,
    /// A `fn`'s head: its body is the first `{` at member level with no `<` open.
    Fn,
    /// An `impl`'s or `trait`'s head: its first `{` at member level with no `<` open opens its
    /// members, one level deeper.
    Container,
    /// Any other member, or a head refused: it ends at a `;` at member level or at the `}`
    /// that closes back to member level.
    Other,
}

/// The open delimiters' codes on [`Members::stack`].
const PAREN: u64 = 1;
const BRACKET: u64 = 2;
const BRACE: u64 = 3;
/// The deepest nesting [`Members::stack`] holds, 2 bits a delimiter.
const MAX_DEPTH: u32 = 32;

/// The member machine of one outline lex.
pub(super) struct Members {
    edition: Edition,
    /// The tokens of the column read so far, and where the next one starts in the text.
    seen: usize,
    offset: usize,
    /// The open delimiters, 2 bits each, the innermost in the low bits.
    stack: u64,
    depth: u32,
    /// The depth members are at: 0, or 1 inside an `impl`'s or a `trait`'s braces.
    member: u32,
    state: State,
    /// The `<`s open at member level in a head.
    angles: u32,
    /// The last token read was `-`, with no trivia after it: a `>` now is `->`'s.
    minus: bool,
    /// The last token that is not trivia ended a `->`.
    arrow: bool,
    /// The last token that is not trivia was an identifier: a `!` now is a macro call's.
    ident: bool,
    /// A close that matched no open, or nesting past [`MAX_DEPTH`]: nothing more is a body.
    off: bool,
}

impl Members {
    pub(super) fn new(edition: Edition) -> Members {
        Members {
            edition,
            seen: 0,
            offset: 0,
            stack: 0,
            depth: 0,
            member: 0,
            state: State::Start,
            angles: 0,
            minus: false,
            arrow: false,
            ident: false,
            off: false,
        }
    }

    /// Reads the tokens `res` holds past those read before.
    pub(super) fn read(&mut self, res: &LexedStr<'_>) {
        let text = res.text;
        while self.seen < res.tokens.len() {
            let (kind, len) = (res.raw_kind(self.seen), res.raw_len(self.seen));
            let start = self.offset;
            self.seen += 1;
            self.offset += len;
            if !self.off {
                self.token(kind, text, start..start + len);
            }
        }
    }

    /// Whether a `{` lexed next opens a `fn` body: a `fn` member's head is read to its end,
    /// with no delimiter and no `<` open in it, and it does not end in `->`.
    pub(super) fn at_body(&self) -> bool {
        !self.off
            && self.state == State::Fn
            && self.depth == self.member
            && self.angles == 0
            && !self.arrow
    }

    /// One token of `kind` at `range` of `text`.
    fn token(&mut self, kind: SyntaxKind, text: &str, range: core::ops::Range<usize>) {
        if kind.is_trivia() {
            self.minus = false;
            return;
        }
        let minus = core::mem::replace(&mut self.minus, kind == MINUS);
        self.arrow = minus && kind == R_ANGLE;
        let ident = core::mem::replace(&mut self.ident, kind == IDENT);
        let at_member = self.depth == self.member;
        match kind {
            L_PAREN => self.open(PAREN),
            L_BRACK => self.open(BRACKET),
            R_PAREN => self.close(PAREN),
            R_BRACK => self.close(BRACKET),
            L_CURLY => self.open_curly(at_member),
            R_CURLY => self.close_curly(),
            LAZY_BODY => match at_member && self.state == State::Fn {
                true => self.state = State::Start,
                false => self.off = true,
            },
            IDENT if self.state == State::Fn && self.is_clause(&text[range.clone()]) => {
                self.state = State::Other
            }
            _ if !at_member => (),
            SEMICOLON => self.state = State::Start,
            _ => self.state = self.at_member(kind, &text[range], minus, ident),
        }
    }

    /// Whether `word`, an identifier's text, starts one of blaze_verify's contract clauses.
    fn is_clause(&self, word: &str) -> bool {
        matches!(
            SyntaxKind::from_contextual_keyword(word, self.edition),
            Some(REQUIRES_KW | ENSURES_KW | RECOMMENDS_KW | DECREASES_KW)
        )
    }

    /// The state after a token at member level that opens, closes and ends nothing; `minus`
    /// and `ident` are what the token before it was.
    fn at_member(&mut self, kind: SyntaxKind, word: &str, minus: bool, ident: bool) -> State {
        match self.state {
            State::Start => match kind {
                POUND | BANG | OUTER_DOC_COMMENT | INNER_DOC_COMMENT | PUB_KW | CONST_KW
                | ASYNC_KW | UNSAFE_KW | EXTERN_KW | STRING | SHEBANG | FRONTMATTER => {
                    State::Start
                }
                IDENT
                    if matches!(
                        SyntaxKind::from_contextual_keyword(word, self.edition),
                        Some(DEFAULT_KW | AUTO_KW | SAFE_KW)
                    ) =>
                {
                    State::Start
                }
                FN_KW => {
                    self.angles = 0;
                    State::Fn
                }
                IMPL_KW | TRAIT_KW if self.member == 0 => {
                    self.angles = 0;
                    State::Container
                }
                _ => State::Other,
            },
            State::Fn | State::Container => match kind {
                L_ANGLE => {
                    self.angles += 1;
                    self.state
                }
                // `->`'s `>`.
                R_ANGLE if minus => self.state,
                R_ANGLE if self.angles == 0 => State::Other,
                R_ANGLE => {
                    self.angles -= 1;
                    self.state
                }
                EQ if self.state == State::Fn && self.angles == 0 => State::Other,
                BANG if ident => State::Other,
                _ => self.state,
            },
            State::Other => State::Other,
        }
    }

    /// A `{`: at member level it opens a `fn` body lexed in full (its `fn` ends where it
    /// closes), an `impl`'s or `trait`'s members, or a brace of another member.
    fn open_curly(&mut self, at_member: bool) {
        let mut members = false;
        let next = match self.state {
            _ if !at_member => self.state,
            State::Fn | State::Container if self.angles > 0 => self.state,
            State::Container if self.member == 0 => {
                members = true;
                State::Start
            }
            State::Start | State::Fn | State::Container | State::Other => State::Other,
        };
        self.open(BRACE);
        if self.off {
            return;
        }
        if members {
            self.member = self.depth;
        }
        self.state = next;
    }

    /// A `}`: the one that closes an `impl`'s or `trait`'s members leaves the item's level;
    /// one that closes back to member level ends the member it is in.
    fn close_curly(&mut self) {
        self.close(BRACE);
        if self.off {
            return;
        }
        if self.member > self.depth {
            // The `impl` or `trait` ends with its members.
            self.member = self.depth;
            self.state = State::Start;
        } else if self.depth == self.member && self.state == State::Other {
            self.state = State::Start;
        }
    }

    fn open(&mut self, code: u64) {
        if self.depth == MAX_DEPTH {
            self.off = true;
            return;
        }
        self.stack = self.stack << 2 | code;
        self.depth += 1;
    }

    fn close(&mut self, code: u64) {
        if self.depth == 0 || self.stack & 3 != code {
            self.off = true;
            return;
        }
        self.stack >>= 2;
        self.depth -= 1;
    }
}
