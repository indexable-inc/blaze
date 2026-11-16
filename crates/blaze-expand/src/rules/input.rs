//! A call's tokens as the matcher reads them: one flat array over the call's streams, each
//! group's close found once, and the parser's words of every token made once a call, so a
//! fragment's parse reads a slice of them.

use blaze_mem::frame::Frame;
use blaze_own::Vec;
use parser::{PrefixEntryPoint, Step, SyntaxKind, T, Tokens};

use super::{At, CallError, Def, DefTok, Engine, First, Msg, SITE, Style};
use crate::{
    call::Call,
    expansion::TokenOrigin,
    tokens::{Delim, Edition, LitKind, Raw, Spacing, TokenKind, TokenStream, parser_edition},
};

/// Where a token of the view is from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Src {
    Input(u32),
    Args(u32),
    /// The parentheses an `attr(..)` rule's input is put in.
    Made,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Tok {
    pub(crate) kind: TokenKind,
    pub(crate) src: Src,
}

/// One level of the view: the tokens `at..end`, the inside of one group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Level {
    pub(crate) at: u32,
    pub(crate) end: u32,
}

impl Level {
    pub(crate) fn is_empty(self) -> bool {
        self.at >= self.end
    }
}

/// A call's input as one flat array of tokens.
pub(crate) struct View<'c, 'f> {
    call: &'c Call<'c>,
    pub(crate) toks: Vec<Tok, &'f Frame>,
    /// For a token that opens a group, the index of its close; else unused.
    close: Vec<u32, &'f Frame>,
    site: Edition,
    /// The parser's words of the tokens, then [`Tokens::PAD`] words `EOF`; empty until a
    /// fragment is parsed.
    words: Vec<u32, &'f Frame>,
    /// For each token, and one past the last, the index of the first word it makes.
    word_at: Vec<u32, &'f Frame>,
    /// A window's words, copied with their padding, when it ends before the last token.
    window: Vec<u32, &'f Frame>,
}

impl<'c, 'f> View<'c, 'f> {
    /// The tokens an arm of `style` matches: an attribute's arguments and item as two
    /// parenthesized groups, else the input.
    pub(crate) fn new_in(call: &'c Call<'c>, style: Style, frame: &'f Frame) -> View<'c, 'f> {
        let input = call.input();
        let args = call.attr_args();
        let extra = match style {
            Style::Attr => 4 + args.map_or(0, TokenStream::len),
            _ => 0,
        };
        let len = input.len() + extra;
        let mut toks = Vec::with_capacity_in(len, frame);
        let made = |kind| Tok { kind, src: Src::Made };
        let stream = |toks: &mut Vec<Tok, &'f Frame>, stream: &TokenStream<'_>, args: bool| {
            for (at, token) in stream.tokens().iter().enumerate() {
                let at = crate::tokens::index(at);
                let src = if args { Src::Args(at) } else { Src::Input(at) };
                toks.push(Tok { kind: token.kind, src });
            }
        };
        match style {
            Style::Attr => {
                toks.push(made(TokenKind::Open(Delim::Paren)));
                if let Some(args) = args {
                    stream(&mut toks, args, true);
                }
                toks.push(made(TokenKind::Close(Delim::Paren)));
                toks.push(made(TokenKind::Open(Delim::Paren)));
                stream(&mut toks, input, false);
                toks.push(made(TokenKind::Close(Delim::Paren)));
            }
            Style::FnLike | Style::Derive => stream(&mut toks, input, false),
        }
        let mut close = Vec::with_capacity_in(toks.len(), frame);
        close.resize(toks.len(), super::NONE);
        let mut open = Vec::new_in(frame);
        for (at, tok) in toks.iter().enumerate() {
            match tok.kind {
                TokenKind::Open(_) => open.push(at),
                TokenKind::Close(_) => {
                    let start = open.pop().expect("the call's streams were checked balanced (`Checked`)");
                    close[start] = crate::tokens::index(at);
                }
                _ => {}
            }
        }
        View {
            call,
            toks,
            close,
            site: call.site.edition,
            words: Vec::new_in(frame),
            word_at: Vec::new_in(frame),
            window: Vec::new_in(frame),
        }
    }

    pub(crate) fn len(&self) -> u32 {
        crate::tokens::index(self.toks.len())
    }

    /// The whole input: the top level.
    pub(crate) fn top(&self) -> Level {
        Level { at: 0, end: self.len() }
    }

    pub(crate) fn kind(&self, at: u32) -> TokenKind {
        self.toks[at as usize].kind
    }

    /// The close of the group opened at `at`.
    pub(crate) fn close_of(&self, at: u32) -> u32 {
        self.close[at as usize]
    }

    /// The inside of the group opened at `at`.
    pub(crate) fn inside(&self, at: u32) -> Level {
        Level { at: at + 1, end: self.close_of(at) }
    }

    /// The token after the tree at `at`.
    pub(crate) fn after_tree(&self, at: u32) -> u32 {
        match self.kind(at) {
            TokenKind::Open(_) => self.close_of(at) + 1,
            _ => at + 1,
        }
    }

    pub(crate) fn text(&self, at: u32) -> &'c str {
        match self.toks[at as usize].src {
            Src::Input(i) => self.call.input().text_at(i as usize),
            Src::Args(i) => self.args().text_at(i as usize),
            Src::Made => "",
        }
    }

    /// [`View::text`] as bytes, read with no UTF-8 check: what a match compares and a copy
    /// writes.
    pub(crate) fn bytes(&self, at: u32) -> &'c [u8] {
        match self.toks[at as usize].src {
            Src::Input(i) => self.call.input().bytes_at(i as usize),
            Src::Args(i) => self.args().bytes_at(i as usize),
            Src::Made => b"",
        }
    }

    fn args(&self) -> &'c TokenStream<'c> {
        self.call.attr_args().expect("a token of the arguments is of an attribute call")
    }

    pub(crate) fn edition(&self, at: u32) -> Edition {
        let (stream, i) = match self.toks[at as usize].src {
            Src::Input(i) => (self.call.input(), i),
            Src::Args(i) => (self.args(), i),
            Src::Made => return self.site,
        };
        stream.edition_of(stream.tokens()[i as usize].ctx)
    }

    /// The origin of the token at `at` where an output token or a diagnostic names it.
    pub(crate) fn origin(&self, at: u32) -> TokenOrigin {
        match self.toks[at as usize].src {
            Src::Input(i) => TokenOrigin::Input(i),
            Src::Args(i) => TokenOrigin::AttrArgs(i),
            Src::Made => SITE,
        }
    }

    /// The source token's spacing, which a written token follows ([`super::transcribe`]).
    pub(crate) fn char(&self, at: u32) -> Option<char> {
        match self.kind(at) {
            TokenKind::Punct { char, .. } => Some(char),
            _ => None,
        }
    }

    /// Whether `level`'s first tree can be the first of an arm whose first location is `first`.
    pub(crate) fn admits(&self, def: Def<'_>, first: First) -> bool {
        let top = self.top();
        match first {
            First::Any => true,
            First::Empty => top.is_empty(),
            _ if top.is_empty() => false,
            First::Crate => {
                matches!(self.kind(0), TokenKind::Ident { .. }) && self.bytes(0) == b"$crate"
            }
            First::Group(delim) => self.kind(0) == TokenKind::Open(delim),
            First::Token(tok) => match (def.kind(tok), self.kind(0)) {
                (TokenKind::Ident { .. }, TokenKind::Ident { .. })
                | (TokenKind::Literal { .. }, TokenKind::Literal { .. }) => {
                    def.bytes(tok) == self.bytes(0)
                }
                (TokenKind::Punct { char: a, .. }, TokenKind::Punct { char: b, .. }) => a == b,
                // An invisible group may hold what an earlier expansion matched.
                (_, TokenKind::Open(Delim::Invisible)) => true,
                _ => false,
            },
        }
    }

    /// How many tokens of punctuation from `at` glue into one, within `level`.
    pub(crate) fn glued(&self, at: u32, end: u32) -> u8 {
        super::glued_len(|i| (i < end).then(|| self.kind(i)), at)
    }

    /// The parser's words, made once a call.
    fn make_words(&mut self) {
        if !self.word_at.is_empty() {
            return;
        }
        let n = crate::tokens::index(self.toks.len());
        let frame: &'f Frame = self.words.allocator();
        let (mut words, mut word_at) = (
            core::mem::replace(&mut self.words, Vec::new_in(frame)),
            core::mem::replace(&mut self.word_at, Vec::new_in(frame)),
        );
        words_of(
            n,
            |at| (self.kind(at), self.text(at), self.edition(at)),
            &mut words,
            &mut word_at,
        );
        (self.words, self.word_at) = (words, word_at);
    }

    /// Whether the fragment at the start of `level` is its one token, by the grammar with no
    /// parse: a plain name (no keyword, contextual or strict, of its edition; not `_`; no
    /// lifetime) as an expression, type, path or pattern, or a literal as an expression or
    /// pattern, followed by nothing, `,` or `;`, which end each of them. What the parser gives
    /// for these, without its cost.
    fn one_token_fragment(&self, entry: PrefixEntryPoint, level: Level) -> bool {
        let at = level.at;
        if at >= level.end {
            return false;
        }
        let next = at + 1;
        let ends = next == level.end || matches!(self.char(next), Some(',' | ';'));
        if !ends {
            return false;
        }
        match self.kind(at) {
            TokenKind::Ident { .. } => {
                let Ok(text) = core::str::from_utf8(self.bytes(at)) else { return false };
                let edition = parser_edition(self.edition(at));
                matches!(
                    entry,
                    PrefixEntryPoint::Expr
                        | PrefixEntryPoint::Ty
                        | PrefixEntryPoint::Path
                        | PrefixEntryPoint::Pat
                        | PrefixEntryPoint::PatTop
                ) && text != "_"
                    && !text.starts_with('\'')
                    && SyntaxKind::from_keyword(text, edition).is_none()
                    && SyntaxKind::from_contextual_keyword(text, edition).is_none()
            }
            TokenKind::Literal { kind, .. } => {
                kind != LitKind::Err
                    && matches!(
                        entry,
                        PrefixEntryPoint::Expr | PrefixEntryPoint::Pat | PrefixEntryPoint::PatTop
                    )
            }
            _ => false,
        }
    }

    /// Parses a fragment of `entry` at the start of `level`: where the input goes on after it,
    /// and the first error, as mbe's `expect_fragment` gives them.
    pub(crate) fn parse_fragment(
        &mut self,
        engine: &mut Engine<'_, '_, 'f>,
        entry: PrefixEntryPoint,
        level: Level,
    ) -> (u32, Option<CallError>) {
        if self.one_token_fragment(entry, level) {
            return (level.at + 1, None);
        }
        self.make_words();
        // The parser reads the level's first trees, a window doubled until it provably read
        // nothing past it (mbe's `expect_fragment`, whose windows these are, so that a parse
        // gives what mbe's does): its position only moves forward, and it reads at most
        // `LOOKAHEAD - 1` tokens past it.
        const LOOKAHEAD: usize = 6;
        let mut trees = 8usize;
        let start = level.at;
        let first_counts =
            !level.is_empty() && self.kind(start) == TokenKind::Open(Delim::Invisible);
        let output = loop {
            let mut end = start;
            let mut taken = 0;
            while taken < trees && end < level.end {
                end = self.after_tree(end);
                taken += 1;
            }
            let whole = end >= level.end;
            let (from, to) = (self.word_at[start as usize] as usize, self.word_at[end as usize] as usize);
            let tokens = match to + Tokens::PAD == self.words.len() {
                true => Tokens::new(&self.words[from..], true),
                false => {
                    self.window.clear();
                    self.window.extend_from_slice(&self.words[from..to]);
                    for _ in 0..Tokens::PAD {
                        self.window.push(Tokens::EOF_WORD);
                    }
                    Tokens::new(&self.window, true)
                }
            };
            let tokens = if first_counts { tokens } else { tokens.without_first_fragment() };
            let len = tokens.len();
            let output = entry.parse(tokens);
            if whole || consumed(&output) + LOOKAHEAD <= len {
                break output;
            }
            trees *= 2;
        };
        let mut cursor = Cursor { at: start, end: level.end, depth: 0 };
        let mut error = None;
        for step in output.iter() {
            match step {
                Step::Token { kind, n_input_tokens } => {
                    let n = match kind {
                        SyntaxKind::LIFETIME_IDENT => 2,
                        _ => n_input_tokens,
                    };
                    for _ in 0..n {
                        cursor.skip_invisible(self);
                        cursor.bump_or_end(self);
                    }
                }
                Step::FloatSplit { .. } => {
                    cursor.skip_invisible(self);
                    cursor.bump_or_end(self);
                }
                Step::Enter { .. } | Step::Exit => {}
                Step::Error { msg } => {
                    if error.is_none() {
                        let at = cursor.tree_origin(self);
                        error = Some(CallError::Binding(at, engine.keep_text(msg)));
                    }
                }
            }
        }
        cursor.leave_invisible(self);
        if error.is_none() && cursor.depth != 0 {
            let at = match level.is_empty() {
                true => SITE,
                false => self.origin(start),
            };
            error = Some(CallError::Binding(at, Msg::Static(expected_entry(entry))));
        }
        while cursor.depth != 0 {
            cursor.bump_or_end(self);
        }
        (cursor.at, error)
    }
}

/// tt's buffer cursor over the flat tokens: `depth` groups entered within the level.
struct Cursor {
    at: u32,
    end: u32,
    depth: u32,
}

impl Cursor {
    fn at_group_end(&self, view: &View<'_, '_>) -> bool {
        self.depth > 0 && matches!(view.kind(self.at), TokenKind::Close(_))
    }

    fn bump_or_end(&mut self, view: &View<'_, '_>) {
        if self.at_group_end(view) {
            self.depth -= 1;
            self.at += 1;
            return;
        }
        if self.at >= self.end {
            return;
        }
        if matches!(view.kind(self.at), TokenKind::Open(_)) {
            self.depth += 1;
        }
        self.at += 1;
    }

    fn leave_invisible(&mut self, view: &View<'_, '_>) {
        while self.depth > 0 && view.kind(self.at) == TokenKind::Close(Delim::Invisible) {
            self.depth -= 1;
            self.at += 1;
        }
    }

    fn skip_invisible(&mut self, view: &View<'_, '_>) {
        loop {
            self.leave_invisible(view);
            if !self.at_group_end(view)
                && self.at < self.end
                && view.kind(self.at) == TokenKind::Open(Delim::Invisible)
            {
                self.depth += 1;
                self.at += 1;
            } else {
                break;
            }
        }
    }

    /// The origin of the tree at the cursor, or the call site at a group's or the level's end.
    fn tree_origin(&self, view: &View<'_, '_>) -> At {
        match self.at_group_end(view) || self.at >= self.end {
            true => SITE,
            false => view.origin(self.at),
        }
    }
}

/// The input tokens a parse consumed.
/// The parser's words a parse took.
pub(crate) fn consumed(output: &parser::Output) -> usize {
    output
        .iter()
        .map(|step| match step {
            Step::Token { n_input_tokens, .. } => n_input_tokens as usize,
            Step::FloatSplit { .. } => 1,
            Step::Enter { .. } | Step::Exit | Step::Error { .. } => 0,
        })
        .sum()
}

/// mbe's `expected {entry:?}`.
fn expected_entry(entry: PrefixEntryPoint) -> &'static str {
    match entry {
        PrefixEntryPoint::Vis => "expected Vis",
        PrefixEntryPoint::Block => "expected Block",
        PrefixEntryPoint::Stmt => "expected Stmt",
        PrefixEntryPoint::Pat => "expected Pat",
        PrefixEntryPoint::PatTop => "expected PatTop",
        PrefixEntryPoint::Ty => "expected Ty",
        PrefixEntryPoint::Expr => "expected Expr",
        PrefixEntryPoint::Path => "expected Path",
        PrefixEntryPoint::Item => "expected Item",
        PrefixEntryPoint::MetaItem => "expected MetaItem",
    }
}

/// The parser's words for `n` tokens, `token(at)` giving each one's kind, text and edition:
/// one word per parser token (a `'` and its name are one lifetime), an invisible group's
/// first word flagged [`parser::FRAGMENT`] and its delimiters none; `word_at[at]` the word
/// token `at` starts (one past the last at `n`), then [`Tokens::PAD`] words of `EOF`. The one
/// conversion of a token stream to the parser's input: the matcher's and a body's.
pub(crate) fn words_of<'s, A: core::alloc::Allocator>(
    n: u32,
    token: impl Fn(u32) -> (TokenKind, &'s str, Edition),
    words: &mut Vec<u32, A>,
    word_at: &mut Vec<u32, A>,
) {
    words.reserve(n as usize + Tokens::PAD);
    word_at.reserve(n as usize + 1);
    let mut fragment_at: Option<usize> = None;
    let mut lifetime = false;
    for at in 0..n {
        if let Some(start) = fragment_at
            && words.len() > start
        {
            words[start] |= parser::FRAGMENT;
            fragment_at = None;
        }
        word_at.push(crate::tokens::index(words.len()));
        let (kind, text, edition) = token(at);
        let edition = parser_edition(edition);
        let plain = |kind: SyntaxKind| parser::word(kind, SyntaxKind::EOF, edition);
        if lifetime {
            lifetime = false;
            // A `'` before no identifier: the parser is given an error token.
            words.push(plain(match kind {
                TokenKind::Ident { .. } => SyntaxKind::LIFETIME_IDENT,
                _ => SyntaxKind::ERROR,
            }));
            continue;
        }
        match kind {
            TokenKind::Punct { char: '\'', .. } => lifetime = true,
            TokenKind::Punct { char, spacing } => {
                let mut word = plain(SyntaxKind::from_char(char).unwrap_or(SyntaxKind::ERROR));
                if spacing == Spacing::Joint {
                    word |= parser::JOINT;
                }
                words.push(word);
            }
            TokenKind::Ident { raw, .. } => {
                let word = if text == "_" {
                    plain(T![_])
                } else if raw == Raw::Yes {
                    plain(match text.starts_with('\'') {
                        true => SyntaxKind::LIFETIME_IDENT,
                        false => SyntaxKind::IDENT,
                    })
                } else if let Some(kind) = SyntaxKind::from_keyword(text, edition) {
                    plain(kind)
                } else if text.starts_with('\'') {
                    plain(SyntaxKind::LIFETIME_IDENT)
                } else {
                    let contextual =
                        SyntaxKind::from_contextual_keyword(text, edition).unwrap_or(SyntaxKind::IDENT);
                    parser::word(SyntaxKind::IDENT, contextual, edition)
                };
                words.push(word);
            }
            TokenKind::Literal { kind, suffix_len, .. } => {
                let syntax = literal_kind(kind);
                let mut word = plain(syntax);
                if syntax == SyntaxKind::FLOAT_NUMBER
                    && !text[..text.len() - suffix_len as usize].ends_with('.')
                {
                    word |= parser::JOINT;
                }
                words.push(word);
            }
            TokenKind::Open(Delim::Paren) => words.push(plain(T!['('])),
            TokenKind::Open(Delim::Brace) => words.push(plain(T!['{'])),
            TokenKind::Open(Delim::Bracket) => words.push(plain(T!['['])),
            TokenKind::Open(Delim::Invisible) => fragment_at = Some(words.len()),
            TokenKind::Close(Delim::Paren) => words.push(plain(T![')'])),
            TokenKind::Close(Delim::Brace) => words.push(plain(T!['}'])),
            TokenKind::Close(Delim::Bracket) => words.push(plain(T![']'])),
            TokenKind::Close(Delim::Invisible) => {}
        }
    }
    word_at.push(crate::tokens::index(words.len()));
    for _ in 0..Tokens::PAD {
        words.push(Tokens::EOF_WORD);
    }
}

fn literal_kind(kind: LitKind) -> SyntaxKind {
    match kind {
        LitKind::Byte => SyntaxKind::BYTE,
        LitKind::Char => SyntaxKind::CHAR,
        LitKind::Integer => SyntaxKind::INT_NUMBER,
        LitKind::Float => SyntaxKind::FLOAT_NUMBER,
        LitKind::Str | LitKind::StrRaw(_) => SyntaxKind::STRING,
        LitKind::ByteStr | LitKind::ByteStrRaw(_) => SyntaxKind::BYTE_STRING,
        LitKind::CStr | LitKind::CStrRaw(_) => SyntaxKind::C_STRING,
        LitKind::Err => SyntaxKind::ERROR,
    }
}

/// Whether two glued punctuations are the same characters.
pub(crate) fn same_puncts(view: &View<'_, '_>, at: u32, len: u8, def: Def<'_>, tok: DefTok, def_len: u8) -> bool {
    len == def_len && (0..u32::from(len)).all(|i| view.char(at + i) == Some(def.char(DefTok(tok.0 + i))))
}
