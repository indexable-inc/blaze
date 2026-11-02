//! Shortcuts that span lexer/parser abstraction.
//!
//! The way Rust works, parser doesn't necessary parse text, and you might
//! tokenize text without parsing it further. So, it makes sense to keep
//! abstract token parsing, and string tokenization as completely separate
//! layers.
//!
//! However, often you do parse text into syntax trees and the glue code for
//! that needs to live somewhere. Rather than putting it to lexer or parser, we
//! use a separate shortcuts module for that.

use std::mem;

use crate::{
    Edition, LexedStr, OutlineStep, Step,
    SyntaxKind::{self, *},
};

#[derive(Debug)]
pub enum StrStep<'a> {
    Token { kind: SyntaxKind, text: &'a str },
    Enter { kind: SyntaxKind },
    Exit,
    Error { msg: &'a str, pos: usize },
}

/// A step of [`LexedStr::intersperse_trivia_outline`]: one of a full parse, or a node an
/// outline left unparsed.
#[derive(Debug)]
pub enum OutlineStrStep<'a> {
    Step(StrStep<'a>),
    /// A lazy node of `kind` over `text`, its tokens not parsed. It starts and ends with a token
    /// that is not trivia: the trivia around it are its parent's, as in a full parse.
    Lazy { kind: SyntaxKind, text: &'a str },
}

impl LexedStr<'_> {
    pub fn to_input(&self, edition: Edition) -> crate::Input {
        let _p = tracing::info_span!("LexedStr::to_input").entered();
        let mut res = crate::Input::with_capacity(self.len());
        self.push_input(&mut res, &mut false, edition);
        res
    }

    /// The input of the lexing of `self`'s text followed by `following`'s, when the lexer ends
    /// a token where `self`'s text ends: `self`'s tokens then `following`'s, neither text lexed
    /// again.
    pub fn to_input_followed_by(&self, following: &LexedStr<'_>, edition: Edition) -> crate::Input {
        let mut res = crate::Input::with_capacity(self.len() + following.len());
        let mut was_joint = false;
        self.push_input(&mut res, &mut was_joint, edition);
        following.push_input(&mut res, &mut was_joint, edition);
        res
    }

    /// Only an identifier's text is sliced (for its contextual keyword) and a float's last byte
    /// read: the other tokens cost their word alone.
    fn push_input(&self, res: &mut crate::Input, was_joint: &mut bool, edition: Edition) {
        let text = self.as_str();
        let mut end = 0;
        for (kind, len) in self.kinds_and_lens() {
            let start = end;
            end += len;
            if kind.is_trivia() {
                *was_joint = false
            } else if kind == SyntaxKind::IDENT {
                res.push_ident(
                    SyntaxKind::from_contextual_keyword(&text[start..end], edition)
                        .unwrap_or(SyntaxKind::IDENT),
                    edition,
                )
            } else {
                if *was_joint {
                    res.was_joint();
                }
                res.push(kind, edition);
                // Tag the token as joint if it is float with a fractional part
                // we use this jointness to inform the parser about what token split
                // event to emit when we encounter a float literal in a field access
                if kind == SyntaxKind::FLOAT_NUMBER {
                    // A float is never empty, and a UTF-8 continuation byte is never `.`.
                    if text.as_bytes()[end - 1] != b'.' {
                        res.was_joint();
                    } else {
                        *was_joint = false;
                    }
                } else {
                    *was_joint = true;
                }
            }
        }
    }

    /// NB: only valid to call with Output from Reparser/TopLevelEntry.
    ///
    /// The sink is generic, not `dyn`: it runs once per token and node, and inlined it is the
    /// tree builder's push.
    pub fn intersperse_trivia<F: FnMut(StrStep<'_>) + ?Sized>(
        &self,
        output: &crate::Output,
        sink: &mut F,
    ) -> bool {
        self.intersperse_trivia_outline(output, &mut |step| match step {
            OutlineStrStep::Step(step) => sink(step),
            OutlineStrStep::Lazy { kind, .. } => {
                panic!("a lazy {kind:?} in a full parse: build an outline with `intersperse_trivia_outline`")
            }
        })
    }

    /// As [`LexedStr::intersperse_trivia`], for the output of an outline
    /// ([`crate::TopEntryPoint::parse_outline`]) too: each node it left unparsed is one
    /// [`OutlineStrStep::Lazy`], with the trivia around it placed as a full parse places them.
    pub fn intersperse_trivia_outline<F: FnMut(OutlineStrStep<'_>) + ?Sized>(
        &self,
        output: &crate::Output,
        sink: &mut F,
    ) -> bool {
        let mut builder =
            Builder { lexed: self, pos: 0, offset: 0, state: State::PendingEnter, sink };

        for event in output.steps() {
            match event {
                OutlineStep::Step(Step::Token { kind, n_input_tokens: n_raw_tokens }) => {
                    builder.token(kind, n_raw_tokens)
                }
                OutlineStep::Step(Step::FloatSplit { ends_in_dot: has_pseudo_dot }) => {
                    builder.float_split(has_pseudo_dot)
                }
                OutlineStep::Step(Step::Enter { kind }) => builder.enter(kind),
                OutlineStep::Step(Step::Exit) => builder.exit(),
                OutlineStep::Step(Step::Error { msg }) => {
                    let text_pos = builder.offset;
                    builder.step(StrStep::Error { msg, pos: text_pos });
                }
                OutlineStep::Lazy { kind, n_input_tokens } => builder.lazy(kind, n_input_tokens),
            }
        }

        match mem::replace(&mut builder.state, State::Normal) {
            State::PendingExit => {
                builder.eat_trivias();
                builder.step(StrStep::Exit);
            }
            State::PendingEnter | State::Normal => unreachable!(),
        }

        // is_eof?
        builder.pos == builder.lexed.len()
    }
}

struct Builder<'a, 'b, F: FnMut(OutlineStrStep<'_>) + ?Sized> {
    lexed: &'a LexedStr<'a>,
    pos: usize,
    /// Where token `pos` starts.
    offset: usize,
    state: State,
    sink: &'b mut F,
}

enum State {
    PendingEnter,
    Normal,
    PendingExit,
}

impl<'a, F: FnMut(OutlineStrStep<'_>) + ?Sized> Builder<'a, '_, F> {
    #[inline]
    fn step(&mut self, step: StrStep<'_>) {
        (self.sink)(OutlineStrStep::Step(step))
    }

    fn token(&mut self, kind: SyntaxKind, n_tokens: u8) {
        match mem::replace(&mut self.state, State::Normal) {
            State::PendingEnter => unreachable!(),
            State::PendingExit => self.step(StrStep::Exit),
            State::Normal => (),
        }
        self.eat_trivias();
        self.do_token(kind, n_tokens as usize);
    }

    fn float_split(&mut self, has_pseudo_dot: bool) {
        match mem::replace(&mut self.state, State::Normal) {
            State::PendingEnter => unreachable!(),
            State::PendingExit => self.step(StrStep::Exit),
            State::Normal => (),
        }
        self.eat_trivias();
        self.do_float_split(has_pseudo_dot);
    }

    fn enter(&mut self, kind: SyntaxKind) {
        match mem::replace(&mut self.state, State::Normal) {
            State::PendingEnter => {
                self.step(StrStep::Enter { kind });
                // No need to attach trivias to previous node: there is no
                // previous node.
                return;
            }
            State::PendingExit => self.step(StrStep::Exit),
            State::Normal => (),
        }

        let n_trivias =
            (self.pos..self.lexed.len()).take_while(|&it| self.lexed.kind(it).is_trivia()).count();
        let leading_trivias = self.pos..self.pos + n_trivias;
        let lexed = self.lexed;
        // clone: a range of token indices, no heap.
        let end = self.offset + leading_trivias.clone().map(|it| lexed.token_len(it)).sum::<usize>();
        let n_attached_trivias = n_attached_trivias(
            kind,
            leading_trivias.rev().scan(end, |end, it| {
                let len = lexed.token_len(it);
                *end -= len;
                Some((lexed.kind(it), &lexed.as_str()[*end..*end + len]))
            }),
        );
        self.eat_n_trivias(n_trivias - n_attached_trivias);
        self.step(StrStep::Enter { kind });
        self.eat_n_trivias(n_attached_trivias);
    }

    fn exit(&mut self) {
        match mem::replace(&mut self.state, State::PendingExit) {
            State::PendingEnter => unreachable!(),
            State::PendingExit => self.step(StrStep::Exit),
            State::Normal => (),
        }
    }

    /// A node the parser left unparsed: `n_tokens` tokens that are not trivia and the trivia
    /// between them. Its leading trivia go to the parent first, as an enter of a kind that
    /// attaches none does (a lazy node is a `fn` body, a `BLOCK_EXPR`); its trailing ones go
    /// where the next step puts them, as after any node.
    fn lazy(&mut self, kind: SyntaxKind, n_tokens: u32) {
        match mem::replace(&mut self.state, State::Normal) {
            State::PendingEnter => unreachable!("a lazy node is never the root"),
            State::PendingExit => self.step(StrStep::Exit),
            State::Normal => (),
        }
        self.eat_trivias();
        let start = self.offset;
        let mut left = n_tokens;
        while left > 0 {
            if !self.lexed.kind(self.pos).is_trivia() {
                left -= 1;
            }
            self.offset += self.lexed.token_len(self.pos);
            self.pos += 1;
        }
        let lexed = self.lexed;
        (self.sink)(OutlineStrStep::Lazy { kind, text: &lexed.as_str()[start..self.offset] });
    }

    fn eat_trivias(&mut self) {
        while self.pos < self.lexed.len() {
            let kind = self.lexed.kind(self.pos);
            if !kind.is_trivia() {
                break;
            }
            self.do_token(kind, 1);
        }
    }

    fn eat_n_trivias(&mut self, n: usize) {
        for _ in 0..n {
            let kind = self.lexed.kind(self.pos);
            assert!(kind.is_trivia());
            self.do_token(kind, 1);
        }
    }

    fn do_token(&mut self, kind: SyntaxKind, n_tokens: usize) {
        let lexed = self.lexed;
        let len = (self.pos..self.pos + n_tokens).map(|it| lexed.token_len(it)).sum::<usize>();
        let text = &lexed.as_str()[self.offset..self.offset + len];
        self.pos += n_tokens;
        self.offset += len;
        self.step(StrStep::Token { kind, text });
    }

    fn do_float_split(&mut self, has_pseudo_dot: bool) {
        let lexed = self.lexed;
        let len = lexed.token_len(self.pos);
        let text = &lexed.as_str()[self.offset..self.offset + len];

        match text.split_once('.') {
            Some((left, right)) => {
                assert!(!left.is_empty());
                self.step(StrStep::Enter { kind: SyntaxKind::NAME_REF });
                self.step(StrStep::Token { kind: SyntaxKind::INT_NUMBER, text: left });
                self.step(StrStep::Exit);

                // here we move the exit up, the original exit has been deleted in process
                self.step(StrStep::Exit);

                self.step(StrStep::Token { kind: SyntaxKind::DOT, text: "." });

                if has_pseudo_dot {
                    assert!(right.is_empty(), "{left}.{right}");
                    self.state = State::Normal;
                } else {
                    assert!(!right.is_empty(), "{left}.{right}");
                    self.step(StrStep::Enter { kind: SyntaxKind::NAME_REF });
                    self.step(StrStep::Token { kind: SyntaxKind::INT_NUMBER, text: right });
                    self.step(StrStep::Exit);

                    // the parser creates an unbalanced start node, we are required to close it here
                    self.state = State::PendingExit;
                }
            }
            None => {
                // illegal float literal which doesn't have dot in form (like 1e0)
                // we should emit an error node here
                self.step(StrStep::Error { msg: "illegal float literal", pos: self.offset });
                self.step(StrStep::Enter { kind: SyntaxKind::ERROR });
                self.step(StrStep::Token { kind: SyntaxKind::FLOAT_NUMBER, text });
                self.step(StrStep::Exit);

                // move up
                self.step(StrStep::Exit);

                self.state = if has_pseudo_dot { State::Normal } else { State::PendingExit };
            }
        }

        self.pos += 1;
        self.offset += len;
    }
}

fn n_attached_trivias<'a>(
    kind: SyntaxKind,
    trivias: impl Iterator<Item = (SyntaxKind, &'a str)>,
) -> usize {
    match kind {
        CONST | ENUM | FN | IMPL | MACRO_CALL | MACRO_DEF | MACRO_RULES | MODULE | RECORD_FIELD
        | STATIC | STRUCT | TRAIT | TUPLE_FIELD | TYPE_ALIAS | UNION | USE | VARIANT
        | EXTERN_CRATE => {
            let mut res = 0;
            let mut trivias = trivias.enumerate().peekable();

            while let Some((i, (kind, text))) = trivias.next() {
                match kind {
                    WHITESPACE if text.contains("\n\n") => {
                        // we check whether the next token is a doc-comment
                        // and skip the whitespace in this case
                        if let Some((COMMENT, peek_text)) = trivias.peek().map(|(_, pair)| pair)
                            && is_outer(peek_text)
                        {
                            continue;
                        }
                        break;
                    }
                    COMMENT => {
                        if is_inner(text) {
                            break;
                        }
                        res = i + 1;
                    }
                    _ => (),
                }
            }
            res
        }
        _ => 0,
    }
}

fn is_outer(text: &str) -> bool {
    if text.starts_with("////") || text.starts_with("/***") {
        return false;
    }
    text.starts_with("///") || text.starts_with("/**")
}

fn is_inner(text: &str) -> bool {
    text.starts_with("//!") || text.starts_with("/*!")
}
