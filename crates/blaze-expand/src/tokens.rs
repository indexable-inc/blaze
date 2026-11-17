//! The portable token form: a flat list of tokens, their text in one table, and their syntax
//! contexts in another. It holds no span, no offset, no interned symbol and no arena index, so
//! two equal token sequences are equal values with equal bytes ([`TokenStream::encode`])
//! whatever file and position they were read at.
//!
//! A stream is built by appending: each token's text goes to the end of the text table, and a
//! context is pushed once (an equal one is found and reused), so a stream is canonical by
//! construction.

use blaze_own::{AVec, Alloc, Vec};

use blaze_id::{ContentId, CrateItemKey, ItemKey, Portable};

use crate::error::{LexError, StreamError, Table};

/// The edition a token is read in: the schema's one `Edition`.
pub use blaze_surface::Edition;

/// `edition` as the parser's, for the lexer and the fragment parser.
pub(crate) fn parser_edition(edition: Edition) -> parser::Edition {
    match edition {
        Edition::E2015 => parser::Edition::Edition2015,
        Edition::E2018 => parser::Edition::Edition2018,
        Edition::E2021 => parser::Edition::Edition2021,
        Edition::E2024 => parser::Edition::Edition2024,
    }
}

/// How an expansion's mark resolves names, and where a macro is defined: the schema's own.
pub use blaze_surface::{DefSite, Transparency};

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

    /// The text in `table` when this range is a whole `str` of it; `None` past its end or off
    /// a character's boundary. For a table from outside (a proc macro's), never trusted.
    pub fn get(self, table: &[u8]) -> Option<&str> {
        let at = self.at as usize;
        let bytes = table.get(at..at.checked_add(self.len as usize)?)?;
        core::str::from_utf8(bytes).ok()
    }
}

/// Appends `text` to `table`, giving its range.
pub(crate) fn push_text(
    table: &mut AVec<'_, u8>,
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
    /// A literal; its text is the inside of the quotes then the suffix, and
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

impl Sink for AVec<'_, u8> {
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// A flat token stream: tokens, their text and their contexts, in the allocator it was made in.
#[derive(Clone, Debug)]
pub struct TokenStream<'a> {
    tokens: AVec<'a, Token>,
    text: AVec<'a, u8>,
    contexts: AVec<'a, CtxNode>,
}

impl<'b> PartialEq<TokenStream<'b>> for TokenStream<'_> {
    /// Equal tokens, text and contexts, wherever each stream is allocated.
    fn eq(&self, other: &TokenStream<'b>) -> bool {
        self.tokens[..] == other.tokens[..]
            && self.text[..] == other.text[..]
            && self.contexts[..] == other.contexts[..]
    }
}

impl Eq for TokenStream<'_> {}

impl<'a> TokenStream<'a> {
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
        let mut open = Vec::new_in(self.allocator());
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

impl<'a> TokenStream<'a> {
    pub fn new_in(alloc: Alloc<'a>) -> TokenStream<'a> {
        TokenStream {
            tokens: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
            contexts: Vec::new_in(alloc),
        }
    }

    /// The allocator the stream's tables are in.
    pub fn allocator(&self) -> Alloc<'a> {
        *self.tokens.allocator()
    }

    /// Appends a token of `kind`, `text` and `ctx`, a context of this stream.
    pub fn push(&mut self, kind: TokenKind, text: &str, ctx: Ctx) -> Result<(), StreamError> {
        self.push_bytes(kind, text.as_bytes(), ctx)
    }

    /// [`TokenStream::push`] of text given as bytes: a `&str`'s, or a whole token's text of a
    /// stream ([`TokenStream::bytes_at`]), so the table holds only whole `&str`s. A copy of a
    /// token reads and writes its text so, with no UTF-8 check.
    pub(crate) fn push_bytes(
        &mut self,
        kind: TokenKind,
        text: &[u8],
        ctx: Ctx,
    ) -> Result<(), StreamError> {
        if let Some(at) = ctx.node()
            && at as usize >= self.contexts.len()
        {
            return Err(StreamError::NoSuchContext { ctx: ctx.0 });
        }
        u32::try_from(self.tokens.len() + 1)
            .map_err(|_| StreamError::TooLarge { table: Table::Tokens })?;
        let too_large = StreamError::TooLarge { table: Table::Text };
        let at = u32::try_from(self.text.len()).map_err(|_| too_large)?;
        let len = u32::try_from(text.len()).map_err(|_| too_large)?;
        at.checked_add(len).ok_or(too_large)?;
        if !text.is_empty() {
            self.text.extend_from_slice(text);
        }
        self.tokens.push(Token { kind, text: Text { at, len }, ctx });
        Ok(())
    }

    /// The text of the token at `at` as bytes, for a copy or a comparison.
    pub(crate) fn bytes_at(&self, at: usize) -> &[u8] {
        let text = self.tokens[at].text;
        &self.text[text.at as usize..text.at as usize + text.len as usize]
    }

    /// Room for `tokens` more tokens and `text` more bytes of text.
    pub(crate) fn reserve(&mut self, tokens: usize, text: usize) {
        self.tokens.reserve(tokens);
        self.text.reserve(text);
    }

    /// [`TokenStream::push`] of a token whose text is `head` then `tail`: a literal's inside
    /// and its suffix, apart in the source by the closing quote.
    pub(crate) fn push_joined(
        &mut self,
        kind: TokenKind,
        head: &str,
        tail: &str,
        ctx: Ctx,
    ) -> Result<(), StreamError> {
        if let Some(at) = ctx.node()
            && at as usize >= self.contexts.len()
        {
            return Err(StreamError::NoSuchContext { ctx: ctx.0 });
        }
        u32::try_from(self.tokens.len() + 1)
            .map_err(|_| StreamError::TooLarge { table: Table::Tokens })?;
        let first = push_text(&mut self.text, head, Table::Text)?;
        let second = push_text(&mut self.text, tail, Table::Text)?;
        let text = Text { at: first.at, len: first.len + second.len };
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
    pub fn import_ctx(
        &mut self,
        from: &TokenStream<'_>,
        ctx: Ctx,
    ) -> Result<Ctx, StreamError> {
        let Some(at) = ctx.node() else { return Ok(ctx) };
        let node =
            *from.contexts.get(at as usize).ok_or(StreamError::NoSuchContext { ctx: ctx.0 })?;
        let parent = self.import_ctx(from, node.parent)?;
        self.context(CtxNode { parent, mark: node.mark })
    }

    /// Appends the token at `at` of `from`, its context imported.
    pub fn push_from(
        &mut self,
        from: &TokenStream<'_>,
        at: usize,
    ) -> Result<(), StreamError> {
        let token = from.tokens[at];
        let ctx = self.import_ctx(from, token.ctx)?;
        self.push(token.kind, from.text_of(&token), ctx)
    }

    /// The tokens `range` of this stream, as a stream of their own in `alloc`.
    pub fn copy_in<'b>(
        &self,
        range: core::ops::Range<usize>,
        alloc: Alloc<'b>,
    ) -> Result<TokenStream<'b>, StreamError> {
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

    /// Where the stream's three tables end now, for [`TokenStream::truncate`].
    pub(crate) fn mark(&self) -> StreamMark {
        StreamMark {
            tokens: self.tokens.len(),
            text: self.text.len(),
            contexts: self.contexts.len(),
        }
    }

    /// Back to `mark`: the tokens pushed since, their text and the contexts made since go. A
    /// mark at or past the end (one taken before an earlier, deeper truncate) changes nothing.
    /// A context made since is one only a token pushed since can name: a context is pushed
    /// once, before the first token of it.
    pub(crate) fn truncate(&mut self, mark: StreamMark) {
        if mark.tokens >= self.tokens.len() {
            return;
        }
        self.tokens.truncate(mark.tokens);
        self.text.truncate(mark.text);
        self.contexts.truncate(mark.contexts);
    }
}

/// The lengths of a stream's tables at one moment ([`TokenStream::mark`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct StreamMark {
    pub(crate) tokens: usize,
    text: usize,
    contexts: usize,
}

/// An index of a table this crate keeps under `u32::MAX` entries ([`TokenStream::push`] refuses
/// more), as a `u32`.
pub(crate) fn index(at: usize) -> u32 {
    u32::try_from(at).expect("a stream's tables are kept under 2^32 entries by `TokenStream::push`")
}

/// The tokens of `text` read in `edition`, each of the root context of that edition: comments
/// and spaces gone (a doc comment is its `#[doc = ".."]`), spacing kept.
///
/// Read straight from the lexer's kinds and lengths into the stream's own text table: no token
/// tree, no span and no interned symbol, so a call shares nothing with another thread and
/// everything it allocates is in `alloc`. Groups and spacing are read as rust-analyzer's token
/// trees read them (a stray closer is punctuation; a group left open is closed at the end),
/// which the tests held it equal to while that path was in the tree.
pub fn lex<'a>(
    text: &str,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<TokenStream<'a>, LexError> {
    use parser::SyntaxKind::{
        INNER_DOC_COMMENT, L_BRACK, L_CURLY, L_PAREN, LIFETIME_IDENT, OUTER_DOC_COMMENT, R_BRACK,
        R_CURLY, R_PAREN, UNDERSCORE,
    };

    let lexed = parser::LexedStr::new(parser_edition(edition), text);
    if lexed.errors().next().is_some() {
        return Err(LexError::Refused { edition });
    }
    let refused = LexError::Refused { edition };
    let ctx = Ctx::root(edition);
    let mut out = TokenStream::new_in(alloc);
    // The groups open, the innermost last.
    let mut open: AVec<'a, Delim> = AVec::new_in(alloc);
    // A literal's text with `\r\n` read as `\n`, or a doc comment's quoted: rare, so one buffer.
    let mut scratch = blaze_own::AString::new_in(alloc);
    let mut last: Pending = None;
    let mut offset = 0usize;
    for i in 0..lexed.len() {
        let kind = lexed.kind(i);
        let start = offset;
        offset += lexed.text_len(i);
        let token = text.get(start..offset).ok_or(refused)?;
        let opens = match kind {
            L_PAREN => Some(Delim::Paren),
            L_CURLY => Some(Delim::Brace),
            L_BRACK => Some(Delim::Bracket),
            _ => None,
        };
        let closes = match kind {
            R_PAREN => Some(Delim::Paren),
            R_CURLY => Some(Delim::Brace),
            R_BRACK => Some(Delim::Bracket),
            _ => None,
        };
        // A closer closes the innermost group it matches and every group inside that one; one
        // that matches none is a punctuation token.
        let depth = closes.and_then(|delim| open.iter().rev().position(|it| *it == delim));
        if let Some(delim) = opens {
            joined_to(&mut out, &mut last, start);
            open.push(delim);
            out.open(delim, ctx)?;
        } else if let Some(depth) = depth {
            for _ in 0..=depth {
                let delim = open.pop().ok_or(refused)?;
                joined_to(&mut out, &mut last, start);
                out.close(delim, ctx)?;
            }
        } else if kind == INNER_DOC_COMMENT || kind == OUTER_DOC_COMMENT {
            joined_to(&mut out, &mut last, start);
            last = None;
            doc_comment(&mut out, &mut scratch, token, text.as_bytes().get(offset) == Some(&b'\n'), kind == INNER_DOC_COMMENT, ctx)
                .ok_or(refused)??;
        } else if kind.is_punct() && kind != UNDERSCORE {
            joined_to(&mut out, &mut last, start);
            let joint = i + 1 < lexed.len() && glues_after(lexed.kind(i + 1));
            let spacing = if joint { Spacing::Joint } else { Spacing::Alone };
            let char = token.chars().next().ok_or(refused)?;
            out.punct(char, spacing, ctx)?;
        } else if kind.is_any_identifier() || kind == UNDERSCORE {
            joined_to(&mut out, &mut last, start);
            let (raw, name) = match token.strip_prefix("r#") {
                Some(name) => (Raw::Yes, name),
                None => (Raw::No, token),
            };
            out.push(TokenKind::Ident { raw, spacing: Spacing::Alone }, name, ctx)?;
            last = Some((out.len() - 1, offset));
        } else if kind.is_literal() {
            joined_to(&mut out, &mut last, start);
            // A literal's value holds LF for each CR LF break (parser::line_break).
            let source = if token.contains('\r') {
                scratch.clear();
                for piece in parser::line_break::without_break_crs(token) {
                    scratch.push_str(piece);
                }
                scratch.as_str()
            } else {
                token
            };
            let (lit, text, suffix) = literal_parts(source);
            let suffix_len = u32::try_from(suffix.len())
                .map_err(|_| StreamError::TooLarge { table: Table::Text })?;
            let kind = TokenKind::Literal { kind: lit, suffix_len, spacing: Spacing::Alone };
            out.push_joined(kind, text, suffix, ctx)?;
            last = Some((out.len() - 1, offset));
        } else if kind == LIFETIME_IDENT {
            joined_to(&mut out, &mut last, start);
            out.punct('\'', Spacing::Joint, ctx)?;
            let name = token.get(1..).ok_or(refused)?;
            out.push(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, name, ctx)?;
            last = Some((out.len() - 1, offset));
        }
        // Anything else (spaces, comments, a shebang) is not a token.
    }
    // A group the text leaves open is closed at its end.
    while let Some(delim) = open.pop() {
        joined_to(&mut out, &mut last, 0usize);
        out.close(delim, ctx)?;
    }
    Ok(out)
}

/// Whether a punctuation token is joint with a next token of `kind`: one that is punctuation
/// alone, or a lifetime (its `'`), as rustc's lexer glues operators.
fn glues_after(kind: parser::SyntaxKind) -> bool {
    use parser::SyntaxKind::{
        AMP, AT, BANG, CARET, COLON, COMMA, DOLLAR, DOT, EQ, L_ANGLE, LIFETIME_IDENT, MINUS,
        PERCENT, PIPE, PLUS, POUND, QUESTION, R_ANGLE, SEMICOLON, SLASH, STAR, TILDE,
    };
    matches!(
        kind,
        EQ | L_ANGLE
            | R_ANGLE
            | BANG
            | AMP
            | PIPE
            | TILDE
            | AT
            | DOT
            | COMMA
            | SEMICOLON
            | COLON
            | POUND
            | DOLLAR
            | QUESTION
            | PLUS
            | MINUS
            | STAR
            | SLASH
            | PERCENT
            | CARET
            | LIFETIME_IDENT
    )
}

/// A literal token's kind, the text inside its quotes (a number's digits) and its suffix (none
/// for `_`). A quoted literal keeps its suffix as rustc's lexer does: a suffix where none is
/// allowed is an error of the literal's use. Text that is no literal is an `Err` literal.
fn literal_parts(text: &str) -> (LitKind, &str, &str) {
    use rustc_lexer::LiteralKind as L;
    let token = rustc_lexer::tokenize(text, rustc_lexer::FrontmatterAllowed::No).next();
    let Some(rustc_lexer::Token { kind: rustc_lexer::TokenKind::Literal { kind, suffix_start }, .. }) =
        token
    else {
        return (LitKind::Err, text, "");
    };
    let raw = |hashes: Option<u8>| hashes.unwrap_or_default();
    let (lit, start, end) = match kind {
        L::Int { .. } => (LitKind::Integer, 0, 0),
        L::Float { .. } => (LitKind::Float, 0, 0),
        L::Char { terminated } => (LitKind::Char, 1, usize::from(terminated)),
        L::Byte { terminated } => (LitKind::Byte, 2, usize::from(terminated)),
        L::Str { terminated } => (LitKind::Str, 1, usize::from(terminated)),
        L::ByteStr { terminated } => (LitKind::ByteStr, 2, usize::from(terminated)),
        L::CStr { terminated } => (LitKind::CStr, 2, usize::from(terminated)),
        L::RawStr { n_hashes } => {
            (LitKind::StrRaw(raw(n_hashes)), 2 + usize::from(raw(n_hashes)), 1 + usize::from(raw(n_hashes)))
        }
        L::RawByteStr { n_hashes } => (
            LitKind::ByteStrRaw(raw(n_hashes)),
            3 + usize::from(raw(n_hashes)),
            1 + usize::from(raw(n_hashes)),
        ),
        L::RawCStr { n_hashes } => (
            LitKind::CStrRaw(raw(n_hashes)),
            3 + usize::from(raw(n_hashes)),
            1 + usize::from(raw(n_hashes)),
        ),
    };
    let (quoted, suffix) = text.split_at(suffix_start as usize);
    let inside = &quoted[start..quoted.len() - end];
    (lit, inside, if suffix == "_" { "" } else { suffix })
}

/// The doc comment `token` as the attribute it stands for, `#[doc = ".."]` (`#![..]` when
/// inner), its text quoted as a proc macro sees it; `None` if it is not a doc comment's text.
fn doc_comment(
    out: &mut TokenStream<'_>,
    scratch: &mut blaze_own::AString<'_>,
    token: &str,
    followed_by_lf: bool,
    inner: bool,
    ctx: Ctx,
) -> Option<Result<(), StreamError>> {
    // A line comment's token ends before its LF, so its break's CR is no part of it; inside, each
    // CR LF is one LF of the value (parser::line_break).
    let token = match token.starts_with("//") {
        true => parser::line_break::line_comment_text(token, followed_by_lf),
        false => token,
    };
    let from_end = if token.starts_with("/*") && token.ends_with("*/") { 2 } else { 0 };
    let body = token.get(3..token.len().checked_sub(from_end)?)?;
    scratch.clear();
    for piece in parser::line_break::without_break_crs(body) {
        for char in piece.escape_debug() {
            scratch.push(char);
        }
    }
    let write = |out: &mut TokenStream<'_>| -> Result<(), StreamError> {
        out.punct('#', Spacing::Alone, ctx)?;
        if inner {
            out.punct('!', Spacing::Alone, ctx)?;
        }
        out.open(Delim::Bracket, ctx)?;
        out.push(TokenKind::Ident { raw: Raw::No, spacing: Spacing::Alone }, "doc", ctx)?;
        out.punct('=', Spacing::Alone, ctx)?;
        let kind = TokenKind::Literal { kind: LitKind::Str, suffix_len: 0, spacing: Spacing::Alone };
        out.push(kind, scratch.as_str(), ctx)?;
        out.close(Delim::Bracket, ctx)
    };
    Some(write(out))
}

/// The tokens inside a delimited group whose inner text is `text` (a function-like call's
/// input as written between its delimiters): [`lex`] of the text, except that the last token
/// is joint with the closing delimiter when it touches it (`Spacing::JointHidden`), as the
/// group's own lexing makes it. This is the one form of a call's input: the names library lexes
/// a call's text with it, and the body's lowering copies the same tokens out of the stream of
/// the body around the call, so one call is one [`crate::call_key`] in both layers.
pub fn lex_inside<'a>(
    text: &str,
    edition: Edition,
    alloc: Alloc<'a>,
) -> Result<TokenStream<'a>, LexError> {
    let mut group = blaze_own::AString::new_in(alloc);
    group.push('(');
    group.push_str(text);
    group.push(')');
    let whole = lex(group.as_str(), edition, alloc)?;
    Ok(whole.copy_in(1..whole.len() - 1, alloc)?)
}

/// The last identifier or literal pushed, and where its text ends: it is joint with the next
/// token if that one starts there.
type Pending = Option<(usize, usize)>;

/// Marks the pending identifier or literal joint when the next token starts where it ends.
fn joined_to(out: &mut TokenStream<'_>, last: &mut Pending, next: usize) {
    if let Some((at, end)) = last.take()
        && end == next
    {
        out.set_spacing(at, Spacing::JointHidden);
    }
}

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
unsafe impl Portable for TokenStream<'_> {}

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
