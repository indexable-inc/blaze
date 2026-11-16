//! The portable token form: a flat list of tokens, their text in one table, and their syntax
//! contexts in another. It holds no span, no offset, no interned symbol and no arena index, so
//! two equal token sequences are equal values with equal bytes ([`TokenStream::encode`])
//! whatever file and position they were read at.
//!
//! A stream is built by appending: each token's text goes to the end of the text table, and a
//! context is pushed once (an equal one is found and reused), so a stream is canonical by
//! construction.

use std::alloc::Allocator;

use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};
use span::{EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, SpanAnchor, SyntaxContext};

use crate::error::{LexError, StreamError, Table};

/// The edition a token is read in.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    E2024,
}

impl Edition {
    const ALL: [Edition; 4] = [Edition::E2015, Edition::E2018, Edition::E2021, Edition::E2024];

    pub(crate) fn byte(self) -> u8 {
        match self {
            Edition::E2015 => 0,
            Edition::E2018 => 1,
            Edition::E2021 => 2,
            Edition::E2024 => 3,
        }
    }

    pub(crate) fn to_span(self) -> span::Edition {
        match self {
            Edition::E2015 => span::Edition::Edition2015,
            Edition::E2018 => span::Edition::Edition2018,
            Edition::E2021 => span::Edition::Edition2021,
            Edition::E2024 => span::Edition::Edition2024,
        }
    }
}

/// How an expansion's mark resolves names (rustc's `Transparency`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum Transparency {
    /// Resolved at the call site (`Span::call_site`, `macro` hygiene opt-outs).
    Transparent,
    /// Locals, labels and `$crate` at the definition, everything else at the call site
    /// (`macro_rules!`, `Span::mixed_site`).
    SemiOpaque,
    /// Resolved at the definition (`macro`, `Span::def_site`).
    Opaque,
}

impl Transparency {
    pub(crate) fn byte(self) -> u8 {
        match self {
            Transparency::Transparent => 0,
            Transparency::SemiOpaque => 1,
            Transparency::Opaque => 2,
        }
    }
}

/// A group's delimiter.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Delim {
    Paren,
    Brace,
    Bracket,
    /// A group no text writes: what a `macro_rules!` fragment is passed on in.
    Invisible,
}

impl Delim {
    fn byte(self) -> u8 {
        match self {
            Delim::Paren => 0,
            Delim::Brace => 1,
            Delim::Bracket => 2,
            Delim::Invisible => 3,
        }
    }

    pub(crate) fn of_tt(kind: tt::DelimiterKind) -> Delim {
        match kind {
            tt::DelimiterKind::Parenthesis => Delim::Paren,
            tt::DelimiterKind::Brace => Delim::Brace,
            tt::DelimiterKind::Bracket => Delim::Bracket,
            tt::DelimiterKind::Invisible => Delim::Invisible,
        }
    }

    pub(crate) fn to_tt(self) -> tt::DelimiterKind {
        match self {
            Delim::Paren => tt::DelimiterKind::Parenthesis,
            Delim::Brace => tt::DelimiterKind::Brace,
            Delim::Bracket => tt::DelimiterKind::Bracket,
            Delim::Invisible => tt::DelimiterKind::Invisible,
        }
    }
}

/// Whether a token is written right against the next one (rustc's `Spacing`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Spacing {
    /// Followed by a space, a comment, or nothing.
    Alone,
    /// A punctuation followed right away by a punctuation: the two may glue.
    Joint,
    /// Followed right away by a token that does not glue to it.
    JointHidden,
}

impl Spacing {
    fn byte(self) -> u8 {
        match self {
            Spacing::Alone => 0,
            Spacing::Joint => 1,
            Spacing::JointHidden => 2,
        }
    }

    pub(crate) fn of_tt(spacing: tt::Spacing) -> Spacing {
        match spacing {
            tt::Spacing::Alone => Spacing::Alone,
            tt::Spacing::Joint => Spacing::Joint,
            tt::Spacing::JointHidden => Spacing::JointHidden,
        }
    }

    pub(crate) fn to_tt(self) -> tt::Spacing {
        match self {
            Spacing::Alone => tt::Spacing::Alone,
            Spacing::Joint => tt::Spacing::Joint,
            Spacing::JointHidden => tt::Spacing::JointHidden,
        }
    }
}

/// A literal's kind; a raw one with its count of `#`s.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LitKind {
    Byte,
    Char,
    Integer,
    Float,
    Str,
    StrRaw(u8),
    ByteStr,
    ByteStrRaw(u8),
    CStr,
    CStrRaw(u8),
    /// A literal the lexer refused, kept so its error is reported where it is used.
    Err,
}

impl LitKind {
    fn bytes(self) -> [u8; 2] {
        match self {
            LitKind::Byte => [0, 0],
            LitKind::Char => [1, 0],
            LitKind::Integer => [2, 0],
            LitKind::Float => [3, 0],
            LitKind::Str => [4, 0],
            LitKind::StrRaw(hashes) => [5, hashes],
            LitKind::ByteStr => [6, 0],
            LitKind::ByteStrRaw(hashes) => [7, hashes],
            LitKind::CStr => [8, 0],
            LitKind::CStrRaw(hashes) => [9, hashes],
            LitKind::Err => [10, 0],
        }
    }

    pub(crate) fn of_tt(kind: tt::LitKind) -> LitKind {
        match kind {
            tt::LitKind::Byte => LitKind::Byte,
            tt::LitKind::Char => LitKind::Char,
            tt::LitKind::Integer => LitKind::Integer,
            tt::LitKind::Float => LitKind::Float,
            tt::LitKind::Str => LitKind::Str,
            tt::LitKind::StrRaw(hashes) => LitKind::StrRaw(hashes),
            tt::LitKind::ByteStr => LitKind::ByteStr,
            tt::LitKind::ByteStrRaw(hashes) => LitKind::ByteStrRaw(hashes),
            tt::LitKind::CStr => LitKind::CStr,
            tt::LitKind::CStrRaw(hashes) => LitKind::CStrRaw(hashes),
            tt::LitKind::Err(()) => LitKind::Err,
        }
    }

    pub(crate) fn to_tt(self) -> tt::LitKind {
        match self {
            LitKind::Byte => tt::LitKind::Byte,
            LitKind::Char => tt::LitKind::Char,
            LitKind::Integer => tt::LitKind::Integer,
            LitKind::Float => tt::LitKind::Float,
            LitKind::Str => tt::LitKind::Str,
            LitKind::StrRaw(hashes) => tt::LitKind::StrRaw(hashes),
            LitKind::ByteStr => tt::LitKind::ByteStr,
            LitKind::ByteStrRaw(hashes) => tt::LitKind::ByteStrRaw(hashes),
            LitKind::CStr => tt::LitKind::CStr,
            LitKind::CStrRaw(hashes) => tt::LitKind::CStrRaw(hashes),
            LitKind::Err => tt::LitKind::Err(()),
        }
    }
}

/// Whether an identifier is written `r#name`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Raw {
    No,
    Yes,
}

/// A token's text: a range of its stream's text table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Text {
    pub at: u32,
    pub len: u32,
}

impl Text {
    pub const EMPTY: Text = Text { at: 0, len: 0 };

    /// The text in `table`, a table this range is of: one that holds only UTF-8 pushed as
    /// `&str` and cut at the ends of what was pushed.
    pub fn of(self, table: &[u8]) -> &str {
        let (at, len) = (self.at as usize, self.len as usize);
        core::str::from_utf8(&table[at..at + len])
            .expect("a text table holds whole `&str`s, and a `Text` is the range of one")
    }
}

/// Appends `text` to `table`, giving its range.
pub(crate) fn push_text<A: Allocator>(
    table: &mut Vec<u8, A>,
    text: &str,
    which: Table,
) -> Result<Text, StreamError> {
    let too_large = StreamError::TooLarge { table: which };
    let at = u32::try_from(table.len()).map_err(|_| too_large)?;
    let len = u32::try_from(text.len()).map_err(|_| too_large)?;
    at.checked_add(len).ok_or(too_large)?;
    table.extend_from_slice(text.as_bytes());
    Ok(Text { at, len })
}

/// What a token is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenKind {
    /// An identifier or keyword, `$crate` included; its text is the name without `r#`.
    Ident { raw: Raw, spacing: Spacing },
    /// One punctuation character; its text is empty.
    Punct { char: char, spacing: Spacing },
    /// A literal; its text is tt's, the inside of the quotes then the suffix, and
    /// `suffix_len` the suffix's length in bytes.
    Literal { kind: LitKind, suffix_len: u32, spacing: Spacing },
    /// A group's opening delimiter; its text is empty.
    Open(Delim),
    /// A group's closing delimiter; its text is empty.
    Close(Delim),
}

impl TokenKind {
    /// The spacing of a leaf; a delimiter is `Alone`.
    pub fn spacing(self) -> Spacing {
        match self {
            TokenKind::Ident { spacing, .. }
            | TokenKind::Punct { spacing, .. }
            | TokenKind::Literal { spacing, .. } => spacing,
            TokenKind::Open(_) | TokenKind::Close(_) => Spacing::Alone,
        }
    }

    /// The same token with `spacing`, where it has one.
    pub(crate) fn with_spacing(self, spacing: Spacing) -> TokenKind {
        match self {
            TokenKind::Ident { raw, .. } => TokenKind::Ident { raw, spacing },
            TokenKind::Punct { char, .. } => TokenKind::Punct { char, spacing },
            TokenKind::Literal { kind, suffix_len, .. } => {
                TokenKind::Literal { kind, suffix_len, spacing }
            }
            TokenKind::Open(_) | TokenKind::Close(_) => self,
        }
    }

    fn encode<S: Sink>(self, out: &mut S) {
        match self {
            TokenKind::Ident { raw, spacing } => {
                out.put(&[0, u8::from(raw == Raw::Yes), spacing.byte()]);
            }
            TokenKind::Punct { char, spacing } => {
                out.put(&[1]);
                out.put(&u32::from(char).to_be_bytes());
                out.put(&[spacing.byte()]);
            }
            TokenKind::Literal { kind, suffix_len, spacing } => {
                out.put(&[2]);
                out.put(&kind.bytes());
                out.put(&suffix_len.to_be_bytes());
                out.put(&[spacing.byte()]);
            }
            TokenKind::Open(delim) => out.put(&[3, delim.byte()]),
            TokenKind::Close(delim) => out.put(&[4, delim.byte()]),
        }
    }
}

/// A syntax context of a stream: a root of an edition, or a context of the stream's table.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ctx(u32);

impl Ctx {
    /// The values below this are the roots, one per edition.
    const ROOTS: u32 = 4;

    /// The root context of `edition`: a token no expansion made.
    pub const fn root(edition: Edition) -> Ctx {
        Ctx(edition as u32)
    }

    /// The edition of a root context.
    pub fn root_edition(self) -> Option<Edition> {
        Edition::ALL.get(self.0 as usize).copied()
    }

    /// The index in its stream's context table, for a context that is not a root.
    pub fn node(self) -> Option<u32> {
        self.0.checked_sub(Ctx::ROOTS)
    }

    fn of_node(index: u32) -> Result<Ctx, StreamError> {
        index
            .checked_add(Ctx::ROOTS)
            .map(Ctx)
            .ok_or(StreamError::TooLarge { table: Table::Contexts })
    }

    /// The context as it is encoded.
    pub fn raw(self) -> u32 {
        self.0
    }
}

/// Where a macro is defined: what `$crate` and a def-site path resolve against.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DefSite {
    /// The crate of the definition, what `$crate` names.
    pub krate: CrateItemKey,
    /// The module of the definition, for a macro whose names resolve there (`macro`, and an
    /// opaque mark); `None` for `macro_rules!`, whose def-site names are its crate's.
    pub module: Option<ItemKey>,
}

/// Which expansion a mark is of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum MarkOf {
    /// The expansion the stream is the output of: wherever the record is placed.
    This,
    /// An earlier expansion, by its record's key ([`crate::ExpansionKey`]).
    Outer(ContentId),
}

/// One expansion's mark on a context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Mark {
    pub of: MarkOf,
    pub transparency: Transparency,
    /// The edition of the definition: the edition the token is read in.
    pub edition: Edition,
    pub site: DefSite,
}

/// A context of a stream's table: a mark over a parent context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CtxNode {
    pub parent: Ctx,
    pub mark: Mark,
}

impl CtxNode {
    fn encode<S: Sink>(self, out: &mut S) {
        out.put(&self.parent.0.to_be_bytes());
        let Mark { of, transparency, edition, site } = self.mark;
        match of {
            MarkOf::This => out.put(&[0]),
            MarkOf::Outer(key) => {
                out.put(&[1]);
                out.put(&key.0);
            }
        }
        out.put(&[transparency.byte(), edition.byte()]);
        out.put(&site.krate.0.0);
        match site.module {
            None => out.put(&[0]),
            Some(module) => {
                out.put(&[1]);
                out.put(&module.0.0);
            }
        }
    }
}

/// What resolves one token's names: its context's innermost mark, and its edition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TokenHygiene {
    /// The innermost mark: `None` for a token no expansion made, which resolves where the
    /// reader places the stream.
    pub mark: Option<Mark>,
    pub edition: Edition,
}

/// One token: what it is, its text and its context.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub text: Text,
    pub ctx: Ctx,
}

/// Where canonical bytes go: a hasher for a key, a buffer for a record.
pub trait Sink {
    fn put(&mut self, bytes: &[u8]);
}

impl Sink for blaze_ids::Hasher {
    fn put(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
}

impl<B: Allocator> Sink for Vec<u8, B> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// A flat token stream: tokens, their text and their contexts, in `A`.
#[derive(Clone, Debug)]
pub struct TokenStream<A: Allocator> {
    tokens: Vec<Token, A>,
    text: Vec<u8, A>,
    contexts: Vec<CtxNode, A>,
}

impl<A: Allocator, B: Allocator> PartialEq<TokenStream<B>> for TokenStream<A> {
    /// Equal tokens, text and contexts, wherever each stream is allocated.
    fn eq(&self, other: &TokenStream<B>) -> bool {
        self.tokens[..] == other.tokens[..]
            && self.text[..] == other.text[..]
            && self.contexts[..] == other.contexts[..]
    }
}

impl<A: Allocator> Eq for TokenStream<A> {}

impl<A: Allocator> TokenStream<A> {
    pub fn tokens(&self) -> &[Token] {
        &self.tokens
    }

    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// The text table every token's [`Text`] is a range of.
    pub fn text_table(&self) -> &[u8] {
        &self.text
    }

    /// A token's text.
    pub fn text_of(&self, token: &Token) -> &str {
        token.text.of(&self.text)
    }

    /// The text of the token at `at`.
    pub fn text_at(&self, at: usize) -> &str {
        self.text_of(&self.tokens[at])
    }

    pub fn contexts(&self) -> &[CtxNode] {
        &self.contexts
    }

    /// The node of a context that is not a root.
    pub fn node(&self, ctx: Ctx) -> Option<CtxNode> {
        ctx.node().and_then(|at| self.contexts.get(at as usize)).copied()
    }

    /// The edition a token of `ctx` is read in: its root's, or its innermost mark's.
    pub fn edition_of(&self, ctx: Ctx) -> Edition {
        match (ctx.root_edition(), self.node(ctx)) {
            (Some(edition), _) => edition,
            (None, Some(node)) => node.mark.edition,
            (None, None) => unreachable!(
                "a token's context is checked to be its stream's when it is pushed (`TokenStream::push`)"
            ),
        }
    }

    /// What resolves the names of a token of `ctx`.
    pub fn hygiene(&self, ctx: Ctx) -> TokenHygiene {
        TokenHygiene { mark: self.node(ctx).map(|node| node.mark), edition: self.edition_of(ctx) }
    }

    /// The index of the delimiter that closes the group opened at `open`.
    pub fn close_of(&self, open: usize) -> Option<usize> {
        let mut depth = 0u32;
        for (at, token) in self.tokens.iter().enumerate().skip(open) {
            match token.kind {
                TokenKind::Open(_) => depth += 1,
                TokenKind::Close(_) => {
                    depth = depth.checked_sub(1)?;
                    if depth == 0 {
                        return Some(at);
                    }
                }
                _ if depth == 0 => return None,
                _ => {}
            }
        }
        None
    }

    /// The first token at which the delimiters do not pair up, if any.
    pub(crate) fn unbalanced(&self) -> Option<u32> {
        let mut open: Vec<Delim, std::alloc::Global> = Vec::new_in(std::alloc::Global);
        for (at, token) in self.tokens.iter().enumerate() {
            match token.kind {
                TokenKind::Open(delim) => open.push(delim),
                TokenKind::Close(delim) => {
                    if open.pop() != Some(delim) {
                        return Some(index(at));
                    }
                }
                _ => {}
            }
        }
        (!open.is_empty()).then(|| index(self.tokens.len()))
    }

    /// Whether a context of the table is this stream's own expansion's ([`MarkOf::This`]).
    pub fn has_own_marks(&self) -> bool {
        self.contexts.iter().any(|node| node.mark.of == MarkOf::This)
    }

    /// The stream's canonical bytes: equal streams, and only they, give equal bytes.
    pub fn encode<S: Sink>(&self, out: &mut S) {
        out.put(&index(self.tokens.len()).to_be_bytes());
        for token in &self.tokens {
            token.kind.encode(out);
            let text = self.text_of(token);
            out.put(&index(text.len()).to_be_bytes());
            out.put(text.as_bytes());
            out.put(&token.ctx.0.to_be_bytes());
        }
        out.put(&index(self.contexts.len()).to_be_bytes());
        for node in &self.contexts {
            node.encode(out);
        }
    }
}

impl<A: Allocator + Clone> TokenStream<A> {
    pub fn new_in(alloc: A) -> TokenStream<A> {
        TokenStream {
            tokens: Vec::new_in(alloc.clone()),
            text: Vec::new_in(alloc.clone()),
            contexts: Vec::new_in(alloc),
        }
    }

    pub fn allocator(&self) -> &A {
        self.tokens.allocator()
    }

    /// Appends a token of `kind`, `text` and `ctx`, a context of this stream.
    pub fn push(&mut self, kind: TokenKind, text: &str, ctx: Ctx) -> Result<(), StreamError> {
        if let Some(at) = ctx.node()
            && at as usize >= self.contexts.len()
        {
            return Err(StreamError::NoSuchContext { ctx: ctx.0 });
        }
        u32::try_from(self.tokens.len() + 1)
            .map_err(|_| StreamError::TooLarge { table: Table::Tokens })?;
        let text = push_text(&mut self.text, text, Table::Text)?;
        self.tokens.push(Token { kind, text, ctx });
        Ok(())
    }

    /// An identifier; `r#name` is pushed as `name`, raw.
    pub fn ident(&mut self, name: &str, ctx: Ctx) -> Result<(), StreamError> {
        let (raw, name) = match name.strip_prefix("r#") {
            Some(name) => (Raw::Yes, name),
            None => (Raw::No, name),
        };
        self.push(TokenKind::Ident { raw, spacing: Spacing::Alone }, name, ctx)
    }

    pub fn punct(&mut self, char: char, spacing: Spacing, ctx: Ctx) -> Result<(), StreamError> {
        self.push(TokenKind::Punct { char, spacing }, "", ctx)
    }

    /// A literal of `kind`: `text` is the inside of its quotes and then its suffix, of
    /// `suffix_len` bytes.
    pub fn literal(
        &mut self,
        kind: LitKind,
        text: &str,
        suffix_len: u32,
        ctx: Ctx,
    ) -> Result<(), StreamError> {
        self.push(TokenKind::Literal { kind, suffix_len, spacing: Spacing::Alone }, text, ctx)
    }

    pub fn open(&mut self, delim: Delim, ctx: Ctx) -> Result<(), StreamError> {
        self.push(TokenKind::Open(delim), "", ctx)
    }

    pub fn close(&mut self, delim: Delim, ctx: Ctx) -> Result<(), StreamError> {
        self.push(TokenKind::Close(delim), "", ctx)
    }

    /// The context `node`: an equal one of the table, or a new one.
    pub fn context(&mut self, node: CtxNode) -> Result<Ctx, StreamError> {
        if let Some(at) = node.parent.node()
            && at as usize >= self.contexts.len()
        {
            return Err(StreamError::NoSuchContext { ctx: node.parent.0 });
        }
        if let Some(at) = self.contexts.iter().position(|it| *it == node) {
            return Ctx::of_node(index(at));
        }
        let ctx = Ctx::of_node(
            u32::try_from(self.contexts.len())
                .map_err(|_| StreamError::TooLarge { table: Table::Contexts })?,
        )?;
        self.contexts.push(node);
        Ok(ctx)
    }

    /// `ctx`, a context of `from`, as a context of this stream: its chain of marks pushed here.
    pub fn import_ctx<B: Allocator>(
        &mut self,
        from: &TokenStream<B>,
        ctx: Ctx,
    ) -> Result<Ctx, StreamError> {
        let Some(at) = ctx.node() else { return Ok(ctx) };
        let node = *from.contexts.get(at as usize).ok_or(StreamError::NoSuchContext { ctx: ctx.0 })?;
        let parent = self.import_ctx(from, node.parent)?;
        self.context(CtxNode { parent, mark: node.mark })
    }

    /// Appends the token at `at` of `from`, its context imported.
    pub fn push_from<B: Allocator>(
        &mut self,
        from: &TokenStream<B>,
        at: usize,
    ) -> Result<(), StreamError> {
        let token = from.tokens[at];
        let ctx = self.import_ctx(from, token.ctx)?;
        self.push(token.kind, from.text_of(&token), ctx)
    }

    /// The tokens `range` of this stream, as a stream of their own in `alloc`.
    pub fn copy_in<B: Allocator + Clone>(
        &self,
        range: core::ops::Range<usize>,
        alloc: B,
    ) -> Result<TokenStream<B>, StreamError> {
        let mut out = TokenStream::new_in(alloc);
        for at in range {
            out.push_from(self, at)?;
        }
        Ok(out)
    }

    /// Names this stream's own expansion by its key: every [`MarkOf::This`] becomes
    /// [`MarkOf::Outer`] of `key`. An expansion's tokens are sealed so before they are the
    /// input of another call.
    pub fn seal(&mut self, key: ContentId) {
        for node in self.contexts.iter_mut() {
            if node.mark.of == MarkOf::This {
                node.mark.of = MarkOf::Outer(key);
            }
        }
    }

    /// Sets the spacing of the token at `at`.
    pub(crate) fn set_spacing(&mut self, at: usize, spacing: Spacing) {
        let token = &mut self.tokens[at];
        token.kind = token.kind.with_spacing(spacing);
    }

    pub(crate) fn clear(&mut self) {
        self.tokens.clear();
        self.text.clear();
        self.contexts.clear();
    }
}

/// An index of a table this crate keeps under `u32::MAX` entries ([`TokenStream::push`] refuses
/// more), as a `u32`.
pub(crate) fn index(at: usize) -> u32 {
    u32::try_from(at).expect("a stream's tables are kept under 2^32 entries by `TokenStream::push`")
}

/// The tokens of `text` read in `edition`, each of the root context of that edition: comments
/// and spaces gone (a doc comment is its `#[doc = ".."]`), spacing kept.
pub fn lex<A: Allocator + Clone>(
    text: &str,
    edition: Edition,
    alloc: A,
) -> Result<TokenStream<A>, LexError> {
    let anchor = SpanAnchor {
        file_id: EditionedFileId::new(FileId::from_raw(0), edition.to_span()),
        ast_id: ROOT_ERASED_FILE_AST_ID,
    };
    let root = SyntaxContext::root(edition.to_span());
    let tree = syntax_bridge::parse_to_token_tree(edition.to_span(), anchor, root, text)
        .ok_or(LexError::Refused { edition })?;
    let mut out = TokenStream::new_in(alloc);
    let mut last = None;
    lex_walk(&mut out, tree.iter(), Ctx::root(edition), &mut last)?;
    Ok(out)
}

/// The last identifier or literal pushed, and where its text ends: it is joint with the next
/// token if that one starts there.
type Pending = Option<(usize, tt::TextSize)>;

fn lex_walk<A: Allocator + Clone>(
    out: &mut TokenStream<A>,
    iter: tt::TtIter<'_>,
    ctx: Ctx,
    last: &mut Pending,
) -> Result<(), StreamError> {
    for element in iter {
        match element {
            tt::TtElement::Leaf(leaf) => {
                let range = leaf.span().range;
                joined_to(out, last, range.start());
                match leaf {
                    tt::Leaf::Ident(ident) => {
                        let raw = if ident.is_raw.yes() { Raw::Yes } else { Raw::No };
                        let kind = TokenKind::Ident { raw, spacing: Spacing::Alone };
                        out.push(kind, ident.sym.as_str(), ctx)?;
                        *last = Some((out.len() - 1, range.end()));
                    }
                    tt::Leaf::Punct(punct) => {
                        out.punct(punct.char, Spacing::of_tt(punct.spacing), ctx)?;
                    }
                    tt::Leaf::Literal(literal) => {
                        let kind = TokenKind::Literal {
                            kind: LitKind::of_tt(literal.kind),
                            suffix_len: literal.suffix_len,
                            spacing: Spacing::Alone,
                        };
                        out.push(kind, literal.text_and_suffix.as_str(), ctx)?;
                        *last = Some((out.len() - 1, range.end()));
                    }
                }
            }
            tt::TtElement::Subtree(subtree, inner) => {
                let delim = Delim::of_tt(subtree.delimiter.kind);
                joined_to(out, last, subtree.delimiter.open.range.start());
                out.open(delim, ctx)?;
                lex_walk(out, inner, ctx, last)?;
                joined_to(out, last, subtree.delimiter.close.range.start());
                out.close(delim, ctx)?;
            }
        }
    }
    Ok(())
}

/// Marks the pending identifier or literal joint when the next token starts where it ends.
fn joined_to<A: Allocator + Clone>(out: &mut TokenStream<A>, last: &mut Pending, next: tt::TextSize) {
    if let Some((at, end)) = last.take()
        && end == next
    {
        out.set_spacing(at, Spacing::JointHidden);
    }
}

// SAFETY: a closed enum of four editions; it names nothing interned.
unsafe impl Portable for Edition {}
// SAFETY: a closed enum; it names nothing interned.
unsafe impl Portable for Transparency {}
// SAFETY: a closed enum; it names nothing interned.
unsafe impl Portable for Delim {}
// SAFETY: a closed enum; it names nothing interned.
unsafe impl Portable for Spacing {}
// SAFETY: a closed enum and a count of `#`s.
unsafe impl Portable for LitKind {}
// SAFETY: a closed enum.
unsafe impl Portable for Raw {}
// SAFETY: a range of the stream's own text table, the same in every process.
unsafe impl Portable for Text {}
// SAFETY: a character, counts and closed enums.
unsafe impl Portable for TokenKind {}
// SAFETY: a root by edition, or an index of the stream's own context table, which travels
// with it: never an interned `SyntaxContext`.
unsafe impl Portable for Ctx {}
// SAFETY: a crate key and a module's item key.
unsafe impl Portable for DefSite {}
// SAFETY: this expansion, or an earlier one by its record's content key.
unsafe impl Portable for MarkOf {}
// SAFETY: every field is `Portable`.
unsafe impl Portable for Mark {}
// SAFETY: every field is `Portable`.
unsafe impl Portable for CtxNode {}
// SAFETY: every field is `Portable`.
unsafe impl Portable for Token {}
// SAFETY: its three tables hold `Token`, UTF-8 bytes and `CtxNode`, each `Portable`; the
// allocator is where the bytes are, not what they mean.
unsafe impl<A: Allocator> Portable for TokenStream<A> {}

/// The bound checks behind the `unsafe impl`s of this module: never called, compiled.
#[expect(dead_code, reason = "compiled for its bounds, never called")]
fn fields_are_portable() {
    fn portable<T: Portable + ?Sized>() {}
    portable::<ContentId>();
    portable::<CrateItemKey>();
    portable::<Option<ItemKey>>();
    portable::<char>();
    portable::<u32>();
    portable::<[u8]>();
}
