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
    tokens::{Delim, Edition, LitKind, Raw, Spacing, TokenKind, TokenStream, WholeText, parser_edition},
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
    /// The parser's words of the window a fragment is parsed in, then [`Tokens::PAD`] words
    /// `EOF`: made for the window's tokens alone, so a parse costs by the fragment's length,
    /// not the input's.
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

    /// [`View::text`] as bytes: what a match compares (a copy writes [`View::whole`]).
    pub(crate) fn bytes(&self, at: u32) -> &'c [u8] {
        match self.toks[at as usize].src {
            Src::Input(i) => self.call.input().bytes_at(i as usize),
            Src::Args(i) => self.args().bytes_at(i as usize),
            Src::Made => b"",
        }
    }

    /// [`View::text`] as a [`WholeText`]: what a copy writes, with no UTF-8 check.
    pub(crate) fn whole(&self, at: u32) -> WholeText<'c> {
        match self.toks[at as usize].src {
            Src::Input(i) => self.call.input().whole_at(i as usize),
            Src::Args(i) => self.args().whole_at(i as usize),
            Src::Made => WholeText::of(""),
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

    /// The parser's words of the tokens `from..to` in `self.window`, padded: what [`words_of`]
    /// gives those tokens in the whole input ([`words_range`]).
    fn make_window(&mut self, from: u32, to: u32) {
        let frame: &'f Frame = self.window.allocator();
        let mut window = core::mem::replace(&mut self.window, Vec::new_in(frame));
        window.clear();
        window.reserve((to - from) as usize + Tokens::PAD);
        let lifetime = lifetime_name_at(|at| self.kind(at), from);
        words_range(from, to, lifetime, |at| (self.kind(at), self.text(at), self.edition(at)), &mut window, None);
        for _ in 0..Tokens::PAD {
            window.push(Tokens::EOF_WORD);
        }
        self.window = window;
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
        // astlog-ignore: expand-hand-extent (the macro language: a separator of `$(..),*`)
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
    /// and the first error, as mbe's `expect_fragment` gives them; and whether it [`Joins`].
    pub(crate) fn parse_fragment(
        &mut self,
        engine: &mut Engine<'_, '_, 'f>,
        entry: PrefixEntryPoint,
        level: Level,
    ) -> (u32, Option<CallError>, Joins) {
        if self.one_token_fragment(entry, level) {
            return (level.at + 1, None, Joins::No);
        }
        // The parser reads the level's first tokens, a window doubled until it provably read
        // nothing past it: its position only moves forward, and it reads at most
        // `LOOKAHEAD - 1` tokens past it, so a parse that ended `LOOKAHEAD` words before the
        // window's end is the parse of the whole level, whatever the window (mbe's
        // `expect_fragment` doubles a window of trees by the same test). The window is of whole
        // trees (a fragment that is a few trees is parsed once), but a tree that would take it
        // past `cap` tokens is left out unless the parse before asked for it (`least`): a
        // large group after a short fragment is not read. Its words are made for it alone, so
        // a parse costs by the fragment's length, not by the trees after it or the input's.
        const LOOKAHEAD: usize = 6;
        const FIRST_TREES: usize = 8;
        const FIRST_TOKENS: u32 = 16;
        let (mut trees, mut cap, mut least) = (FIRST_TREES, FIRST_TOKENS, 1usize);
        // A visibility, type, path or attribute's inside is short and often stands before a
        // large group it does not reach; an item, block, statement, expression or pattern
        // usually ends in its large group, so leaving that out would only add a parse.
        let short = matches!(
            entry,
            PrefixEntryPoint::Vis | PrefixEntryPoint::Ty | PrefixEntryPoint::Path | PrefixEntryPoint::MetaItem
        );
        let start = level.at;
        let first_counts =
            !level.is_empty() && self.kind(start) == TokenKind::Open(Delim::Invisible);
        let output = loop {
            let mut end = start;
            let mut taken = 0;
            while taken < trees && end < level.end {
                let next = self.after_tree(end);
                if short && taken >= least && next - start > cap {
                    break;
                }
                end = next;
                taken += 1;
            }
            let whole = end >= level.end;
            self.make_window(start, end);
            let tokens = Tokens::new(&self.window, true);
            let tokens = if first_counts { tokens } else { tokens.without_first_fragment() };
            let len = tokens.len();
            let output = entry.parse(tokens);
            if whole || consumed(&output) + LOOKAHEAD <= len {
                break output;
            }
            trees = trees.saturating_mul(2);
            cap = cap.saturating_mul(2);
            least = taken + 1;
        };
        let mut cursor = Cursor { at: start, end: level.end, depth: 0 };
        let mut error = None;
        // The fragment's node (the first entered), the node at depth 2 being read, the depth.
        let (mut root, mut second, mut depth) = (None, None, 0u32);
        let mut joins = Joins::No;
        for step in output.iter() {
            match step {
                Step::Enter { kind } => {
                    depth += 1;
                    match depth {
                        1 if root.is_none() => root = Some(kind),
                        2 => second = Some(kind),
                        _ => {}
                    }
                    // `A | B`, `| A`: an or-pattern is a list of `|`.
                    if depth == 1 && kind == SyntaxKind::OR_PAT {
                        joins = Joins::Yes;
                    }
                }
                Step::Exit => depth = depth.saturating_sub(1),
                Step::Token { kind, n_input_tokens } => {
                    // `A + B`, `dyn A + B`, `impl A + B`: a `+` of the type's bounds.
                    if depth == 2
                        && kind == T![+]
                        && second == Some(SyntaxKind::TYPE_BOUND_LIST)
                        && matches!(root, Some(SyntaxKind::DYN_TRAIT_TYPE | SyntaxKind::IMPL_TRAIT_TYPE))
                    {
                        joins = Joins::Yes;
                    }
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
        (cursor.at, error, joins)
    }
}

/// Whether a fragment the parser read is a list that a parse of the output would join with
/// the tokens around it where it is put: an or-pattern (`A | B` in `&$p`) or a type of bounds
/// (`A + B` in `&$t`). rustc reads a fragment as one operand (its invisible group is one
/// token tree, parsed whole); the transcriber puts such a fragment in parentheses, as it does
/// an expression (`2 * $e` of `1 + 2` is `2 * (1 + 2)`: transcribe.rs `var`, mbe's), so every
/// parse of the output, a body's lowering and a derive's input alike, reads it whole.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Joins {
    No,
    Yes,
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
    words_range(0, n, false, token, words, Some(word_at));
    word_at.push(crate::tokens::index(words.len()));
    for _ in 0..Tokens::PAD {
        words.push(Tokens::EOF_WORD);
    }
}

/// Whether the token at `at` is a lifetime's name to [`words_of`]: the tokens just before it
/// are an odd number of `'`. A `'` makes the next token its name whatever that is, and a name
/// (another `'` too) makes nothing of the token after it, so the conversion's state before a
/// token is the parity of the run of `'` that ends there.
pub(crate) fn lifetime_name_at(kind: impl Fn(u32) -> TokenKind, at: u32) -> bool {
    let mut run = 0u32;
    while run < at && matches!(kind(at - 1 - run), TokenKind::Punct { char: '\'', .. }) {
        run += 1;
    }
    run % 2 == 1
}

/// [`words_of`] for the tokens `from..to` alone, appended to `words` with no padding: the same
/// words the whole conversion gives those tokens, given `lifetime`
/// ([`lifetime_name_at`] of `from`), but for one flag: a word right after an invisible group
/// that ends at `from` is not flagged [`parser::FRAGMENT`] here (a reader of a window does not
/// count its first token's flag: `Tokens::without_first_fragment`).
fn words_range<'s, A: core::alloc::Allocator>(
    from: u32,
    to: u32,
    mut lifetime: bool,
    token: impl Fn(u32) -> (TokenKind, &'s str, Edition),
    words: &mut Vec<u32, A>,
    mut word_at: Option<&mut Vec<u32, A>>,
) {
    let mut fragment_at: Option<usize> = None;
    for at in from..to {
        if let Some(start) = fragment_at
            && words.len() > start
        {
            words[start] |= parser::FRAGMENT;
            fragment_at = None;
        }
        if let Some(word_at) = word_at.as_deref_mut() {
            word_at.push(crate::tokens::index(words.len()));
        }
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
    // A range that ends on an invisible group's first word (a window cut inside the group; a
    // whole stream is balanced, so its group goes on to its close): flagged as the token
    // after would have.
    if let Some(start) = fragment_at
        && words.len() > start
    {
        words[start] |= parser::FRAGMENT;
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

#[cfg(test)]
mod tests {
    use blaze_id::{ContentId, CrateItemKey};

    use super::*;
    use crate::{CallSite, Ctx, DefAt, DefSite, Export, LocalInner, MacroDefinition, lex};

    /// The Reference, "Macros By Example", "Transcribing": "When matching, no lookahead is
    /// performed; if the compiler cannot unambiguously determine how to parse the macro
    /// invocation one token at a time, then it is an error." A fragment is so decided by the
    /// tokens it starts at, and its cost is theirs: a `vis` of four tokens before 80,000 more
    /// makes the parser words of its first window (16 tokens), not the input's. Expected:
    /// "test rules::input::tests::a_fragment_costs_by_its_length_not_the_inputs ... ok"; with
    /// the first window made the whole input (`FIRST_TREES = usize::MAX`, `FIRST_TOKENS =
    /// u32::MAX`) it fails: "a `vis` of 4 tokens made 80004 parser words of an input of 80004
    /// tokens".
    #[test]
    fn a_fragment_costs_by_its_length_not_the_inputs() {
        let alloc = blaze_own::testing::heap().alloc();
        let mut text = Vec::new_in(alloc);
        text.extend_from_slice(b"pub(crate) ");
        for _ in 0..40_000 {
            text.extend_from_slice(b"x , ");
        }
        let text = core::str::from_utf8(&text).expect("the text is ASCII");
        let input = lex(text, Edition::E2021, alloc).expect("the input lexes");
        let body = lex("() => {}", Edition::E2021, alloc).expect("the rules lex");
        let at = DefAt {
            site: DefSite { krate: CrateItemKey(ContentId([1; 32])), module: None },
            edition: Edition::E2021,
            export: Export::Exported,
            local_inner: LocalInner::No,
            transparency: None,
        };
        let def = MacroDefinition::rules(body, at);
        let program = super::super::compile(&def).expect("the rules compile");
        let site = CallSite { edition: Edition::E2021, panic_edition: Edition::E2021, ctx: Ctx::root(Edition::E2021) };
        let call = Call::fn_like(&input, site);
        blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
            let mut engine = Engine::new_in(&program, Def::of(&def), frame);
            let mut view = View::new_in(&call, Style::FnLike, frame);
            let top = view.top();
            let (end, error, _) = view.parse_fragment(&mut engine, PrefixEntryPoint::Vis, top);
            assert_eq!((end, error.is_none()), (4, true), "`pub(crate)` is the `vis`");
            let made = view.window.len() - Tokens::PAD;
            assert!(made <= 16, "a `vis` of 4 tokens made {made} parser words of an input of {} tokens", view.len());
        });
    }

    /// A window's words are the whole conversion's words of its tokens, wherever it is cut:
    /// between a `'` and its name, after a run of `'`, inside an invisible group, after an
    /// empty one. The one difference allowed is the first word's fragment flag when the window
    /// does not start at an invisible group's open (its reader does not count it). Expected:
    /// "test rules::input::tests::a_windows_words_are_the_whole_conversions ... ok".
    #[test]
    fn a_windows_words_are_the_whole_conversions() {
        let punct = |char| TokenKind::Punct { char, spacing: Spacing::Alone };
        let joint = |char| TokenKind::Punct { char, spacing: Spacing::Joint };
        let ident = TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone };
        let (open, close) = (TokenKind::Open(Delim::Invisible), TokenKind::Close(Delim::Invisible));
        let toks: [(TokenKind, &str); 22] = [
            (punct('\''), ""),
            (ident, "a"),
            (punct('\''), ""),
            (punct('\''), ""),
            (ident, "x"),
            (open, ""),
            (close, ""),
            (ident, "y"),
            (open, ""),
            (ident, "z"),
            (close, ""),
            (TokenKind::Open(Delim::Paren), ""),
            (punct('\''), ""),
            (punct('\''), ""),
            (punct('\''), ""),
            (ident, "b"),
            (TokenKind::Close(Delim::Paren), ""),
            (ident, "fn"),
            (joint(':'), ""),
            (punct(':'), ""),
            (ident, "union"),
            (punct('\''), ""),
        ];
        let n = toks.len() as u32;
        let token = |at: u32| (toks[at as usize].0, toks[at as usize].1, Edition::E2021);
        let alloc = blaze_own::testing::heap().alloc();
        let (mut whole, mut word_at) = (Vec::new_in(alloc), Vec::new_in(alloc));
        words_of(n, token, &mut whole, &mut word_at);
        let mut windows = 0;
        for from in 0..n {
            for to in from..=n {
                let mut window = Vec::new_in(alloc);
                let lifetime = lifetime_name_at(|at| toks[at as usize].0, from);
                words_range(from, to, lifetime, token, &mut window, None);
                let mut expected = Vec::new_in(alloc);
                expected.extend_from_slice(&whole[word_at[from as usize] as usize..word_at[to as usize] as usize]);
                if toks[from as usize].0 != open {
                    if let Some(first) = window.first_mut() {
                        *first &= !parser::FRAGMENT;
                    }
                    if let Some(first) = expected.first_mut() {
                        *first &= !parser::FRAGMENT;
                    }
                }
                assert_eq!(&window[..], &expected[..], "the window {from}..{to}");
                windows += 1;
            }
        }
        assert_eq!(windows, 22 * 23 / 2 + 22);
    }
}
