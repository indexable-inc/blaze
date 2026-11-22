//! [`TokensBuilder`]: what an expander writes its output with, one token at a time, then
//! [`TokensBuilder::finish`] writes the record's bytes once. Everything [`crate::Tokens::open`]
//! checks is checked here first, by the same functions, so a finished record always opens.

use core::fmt;
use core::hash::BuildHasher;
use core::mem::offset_of;

use blaze_format_core::{Own, Placed, Writer, WriterOpts};
use blaze_hash::{FixedState, HashMap};
use blaze_own::{AVec, Alloc, Vec};

use crate::form::{
    Ctx, CtxNode, MAX_TOKENS, NO_LEXEME, NodeRow, Origin, PAD, TOKENS_SCHEMA, TokenRange,
    TokensRoot, check_word, lexeme_span, needs_lexeme, wide,
};

/// A finished token record's bytes, in the builder's allocator.
pub type TokensBytes<'b> = AVec<'b, u8>;

/// One token as the writer hands it over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NewToken<'t> {
    /// The parser's word for it, exactly as `parser::Tokens` takes it: kind, contextual kind,
    /// edition, and the [`parser::JOINT`], [`parser::FRAGMENT`] and syntax-extension bits the
    /// writer's conversion sets (blaze-expand `rules::words_of`).
    pub word: u32,
    /// Its text as a lexer reads it from source: an identifier with its `r#`, a lifetime with
    /// its `'`, a literal with its quotes, prefix and suffix. `None`, or the kind's own text,
    /// for a kind whose text is fixed (a punctuation, a delimiter, a keyword).
    pub lexeme: Option<&'t str>,
    /// Whether it is written right against the next token (rustc's `Spacing::Joint` or
    /// `JointHidden`): the token's own fact, never read off a text.
    pub joined: bool,
    /// Its hygiene context: a root, or one [`TokensBuilder::context`] gave.
    pub ctx: Ctx,
    pub origin: Origin,
}

/// Why the builder refused a token, a context or a group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BuildError {
    /// Token `token`'s word is none the parser takes: `what` says why.
    Word { token: u32, what: &'static str },
    /// Token `token` has no lexeme where its kind needs one, or one its kind's text is not.
    Lexeme { token: u32, what: &'static str },
    /// Token `token` names a context the builder has not made.
    Context { token: u32 },
    /// A context whose parent the builder has not made.
    Parent,
    /// Group `row` (in the order given): `what` says why.
    Group { row: u32, what: &'static str },
    /// More `what` than a record's `u32` indexes reach.
    TooLarge { what: &'static str },
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::Word { token, what } => write!(f, "token record, token {token}: {what}"),
            BuildError::Lexeme { token, what } => write!(f, "token record, token {token}: {what}"),
            BuildError::Context { token } => {
                write!(f, "token record, token {token}: a context the builder has not made")
            }
            BuildError::Parent => {
                f.write_str("token record: a context whose parent the builder has not made")
            }
            BuildError::Group { row, what } => write!(f, "token record, group {row}: {what}"),
            BuildError::TooLarge { what } => {
                write!(f, "token record: more {what} than a u32 index reaches")
            }
        }
    }
}

impl core::error::Error for BuildError {}

/// The writer of one token record, every column in `alloc`.
pub struct TokensBuilder<'b> {
    words: AVec<'b, u32>,
    lexeme_of: AVec<'b, u32>,
    ctxs: AVec<'b, u32>,
    origin_values: AVec<'b, u32>,
    origin_tags: AVec<'b, u8>,
    joined: AVec<'b, u64>,
    lexeme_ends: AVec<'b, u32>,
    lexeme_bytes: AVec<'b, u8>,
    /// A lexeme's hash to the last lexeme of that hash; `chain[k]` the one before `k`.
    lexeme_heads: HashMap<u64, u32, Alloc<'b>>,
    chain: AVec<'b, u32>,
    nodes: AVec<'b, NodeRow>,
    groups: AVec<'b, TokenRange>,
    alloc: Alloc<'b>,
}

impl<'b> TokensBuilder<'b> {
    pub fn new_in(alloc: Alloc<'b>) -> TokensBuilder<'b> {
        TokensBuilder {
            words: Vec::new_in(alloc),
            lexeme_of: Vec::new_in(alloc),
            ctxs: Vec::new_in(alloc),
            origin_values: Vec::new_in(alloc),
            origin_tags: Vec::new_in(alloc),
            joined: Vec::new_in(alloc),
            lexeme_ends: Vec::new_in(alloc),
            lexeme_bytes: Vec::new_in(alloc),
            lexeme_heads: HashMap::new_in(alloc),
            chain: Vec::new_in(alloc),
            nodes: Vec::new_in(alloc),
            groups: Vec::new_in(alloc),
            alloc,
        }
    }

    /// How many tokens are pushed.
    pub fn len(&self) -> usize {
        self.lexeme_of.len()
    }

    /// The context of `node`: the one made before for an equal node, else a new one after
    /// every earlier one, so the table is a function of the order contexts are first asked
    /// for.
    pub fn context(&mut self, node: CtxNode) -> Result<Ctx, BuildError> {
        if !Ctx::is_in(node.parent.raw(), self.nodes.len()) {
            return Err(BuildError::Parent);
        }
        let row = NodeRow::of(node);
        let index = match self.nodes.iter().position(|made| *made == row) {
            Some(index) => index,
            None => {
                self.nodes.push(row);
                self.nodes.len() - 1
            }
        };
        let index = u32::try_from(index)
            .ok()
            .filter(|&index| index < MAX_TOKENS)
            .ok_or(BuildError::TooLarge { what: "contexts" })?;
        Ok(Ctx::of_node(index))
    }

    /// Appends `token`, giving its index.
    pub fn push(&mut self, token: NewToken<'_>) -> Result<u32, BuildError> {
        let index = u32::try_from(self.len())
            .ok()
            .filter(|&index| index < MAX_TOKENS)
            .ok_or(BuildError::TooLarge { what: "tokens" })?;
        let kind = check_word(token.word).map_err(|what| BuildError::Word { token: index, what })?;
        let lexeme = match (needs_lexeme(kind), token.lexeme) {
            (true, Some(text)) => self.lexeme(text)?,
            (true, None) => {
                return Err(BuildError::Lexeme {
                    token: index,
                    what: "no lexeme for a token whose kind's text is not fixed",
                });
            }
            (false, None) => NO_LEXEME,
            (false, Some(text)) if kind.fixed_text() == Some(text) => NO_LEXEME,
            (false, Some(_)) => {
                return Err(BuildError::Lexeme {
                    token: index,
                    what: "a lexeme other than the fixed text of the token's kind",
                });
            }
        };
        if !Ctx::is_in(token.ctx.raw(), self.nodes.len()) {
            return Err(BuildError::Context { token: index });
        }
        let (tag, value) = token.origin.parts();
        self.words.push(token.word);
        self.lexeme_of.push(lexeme);
        self.ctxs.push(token.ctx.raw());
        self.origin_values.push(value);
        self.origin_tags.push(tag);
        let bit = self.len() - 1;
        if bit % 64 == 0 {
            self.joined.push(0);
        }
        if token.joined {
            let word = self.joined.last_mut().expect("a joined word per 64 tokens");
            *word |= 1u64 << (bit % 64);
        }
        Ok(index)
    }

    /// An invisible group over `range`: a `macro_rules!` fragment the expansion passes on,
    /// which a nested call's matcher takes as one. Groups come sorted by start, a group before
    /// those inside it; their ends are checked against the tokens at [`TokensBuilder::finish`].
    pub fn group(&mut self, range: TokenRange) -> Result<(), BuildError> {
        let row = u32::try_from(self.groups.len()).map_err(|_| BuildError::TooLarge { what: "groups" })?;
        if range.end < range.start {
            return Err(BuildError::Group { row, what: "a range that ends before it starts" });
        }
        if let Some(last) = self.groups.last()
            && !in_order(*last, range)
        {
            return Err(BuildError::Group {
                row,
                what: "a group before one it follows: groups are sorted by start, outer first",
            });
        }
        self.groups.push(range);
        Ok(())
    }

    /// The record's bytes in the builder's allocator: equal tokens, contexts and groups pushed
    /// in the same order give equal bytes (rule 25).
    pub fn finish(mut self) -> Result<TokensBytes<'b>, BuildError> {
        let tokens = self.len();
        for (row, group) in self.groups.iter().enumerate() {
            if crate::form::at(group.end) > tokens {
                let row = u32::try_from(row).map_err(|_| BuildError::TooLarge { what: "groups" })?;
                return Err(BuildError::Group { row, what: "a group past the last token" });
            }
        }
        for _ in 0..PAD {
            self.words.push(parser::Tokens::EOF_WORD);
        }
        let mut out = Writer::new(WriterOpts { schema: TOKENS_SCHEMA, alloc: Own(self.alloc), capacity: None });
        let words = Placed::push(&mut out, &self.words);
        let lexeme_of = Placed::push(&mut out, &self.lexeme_of);
        let ctxs = Placed::push(&mut out, &self.ctxs);
        let origin_values = Placed::push(&mut out, &self.origin_values);
        let origin_tags = Placed::push(&mut out, &self.origin_tags);
        let joined = Placed::push(&mut out, &self.joined);
        let lexeme_ends = Placed::push(&mut out, &self.lexeme_ends);
        let lexeme_bytes = Placed::push(&mut out, &self.lexeme_bytes);
        let nodes = Placed::push(&mut out, &self.nodes);
        let groups = Placed::push(&mut out, &self.groups);
        let at = out.next::<TokensRoot>();
        let field = |offset: usize| at + wide(offset);
        let root = TokensRoot {
            words: words.slice(field(offset_of!(TokensRoot, words))),
            lexeme_of: lexeme_of.slice(field(offset_of!(TokensRoot, lexeme_of))),
            ctxs: ctxs.slice(field(offset_of!(TokensRoot, ctxs))),
            origin_values: origin_values.slice(field(offset_of!(TokensRoot, origin_values))),
            origin_tags: origin_tags.slice(field(offset_of!(TokensRoot, origin_tags))),
            joined: joined.slice(field(offset_of!(TokensRoot, joined))),
            lexeme_ends: lexeme_ends.slice(field(offset_of!(TokensRoot, lexeme_ends))),
            lexeme_bytes: lexeme_bytes.slice(field(offset_of!(TokensRoot, lexeme_bytes))),
            nodes: nodes.slice(field(offset_of!(TokensRoot, nodes))),
            groups: groups.slice(field(offset_of!(TokensRoot, groups))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at);
        Ok(out.finish(at))
    }

    /// The number of lexeme `text`: the one it was given when first seen, else the next.
    fn lexeme(&mut self, text: &str) -> Result<u32, BuildError> {
        let hash = FixedState.hash_one(text.as_bytes());
        let mut seen = self.lexeme_heads.get(&hash).copied();
        while let Some(k) = seen {
            let k = crate::form::at(k);
            if &self.lexeme_bytes[lexeme_span(&self.lexeme_ends, k)] == text.as_bytes() {
                return u32::try_from(k).map_err(|_| BuildError::TooLarge { what: "lexemes" });
            }
            seen = match self.chain[k] {
                NO_LEXEME => None,
                before => Some(before),
            };
        }
        let k = u32::try_from(self.lexeme_ends.len())
            .ok()
            .filter(|&k| k < NO_LEXEME)
            .ok_or(BuildError::TooLarge { what: "lexemes" })?;
        let end = self
            .lexeme_bytes
            .len()
            .checked_add(text.len())
            .and_then(|end| u32::try_from(end).ok())
            .ok_or(BuildError::TooLarge { what: "lexeme bytes" })?;
        self.lexeme_bytes.extend_from_slice(text.as_bytes());
        self.lexeme_ends.push(end);
        let before = self.lexeme_heads.insert(hash, k);
        self.chain.push(before.unwrap_or(NO_LEXEME));
        Ok(k)
    }
}

/// Whether group `next` may follow group `last`: a later start, or the same start and an end
/// no later (the outer group first).
pub(crate) fn in_order(last: TokenRange, next: TokenRange) -> bool {
    last.start < next.start || (last.start == next.start && next.end <= last.end)
}
