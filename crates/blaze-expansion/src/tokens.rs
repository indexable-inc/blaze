//! Token streams as keys and as values, with no absolute position in either (AGENTS.md rule 71).
//!
//! A span is given as its anchor (an opaque id of the item it is relative to: in hir-expand, a
//! `span::SpanAnchor`, the file and the ast id), its range from that item's start
//! ([`RelativeRange`]: what `span::Span::range` already is), and its syntax context (opaque).
//! Hashing numbers each anchor and each context by first appearance ([`Numbering`]), so:
//!
//! - an edit above the call moves the anchor's start and nothing in the hash;
//! - two databases or revisions that number anchors and contexts otherwise hash one stream
//!   equally;
//! - any change to a token (its kind, its text, its range, which tokens share an anchor or a
//!   context) changes the hash.
//!
//! The same numbering writes an expansion's output ([`OutputWriter`]): each output span names
//! an ordinal of the call's numbering, never an id of this process, and a context the
//! expansion made (a declarative macro's mark) is a [`Mark`] over an ordinal, which the
//! wiring resolves to a context when it reads the output back.

use std::{alloc::Global, fmt};

use blaze_ids::Hasher;

use crate::{
    Digest,
    flat::{Corrupt, Reader, Why, Writer},
};

/// A byte offset in a file: printed or emitted only, never in a key or a record. The one way
/// to a [`RelativeRange`] is [`RelativeRange::from_absolute`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct AbsoluteOffset(pub u32);

/// A range from the start of the item a span is anchored to.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RelativeRange {
    pub start: u32,
    pub end: u32,
}

impl RelativeRange {
    /// The range `start..end` of a file, relative to an anchor that starts at `anchor`; `None`
    /// for a range that begins before the anchor or ends before it begins.
    pub fn from_absolute(
        start: AbsoluteOffset,
        end: AbsoluteOffset,
        anchor: AbsoluteOffset,
    ) -> Option<RelativeRange> {
        let start = start.0.checked_sub(anchor.0)?;
        let end = end.0.checked_sub(anchor.0)?;
        (start <= end).then_some(RelativeRange { start, end })
    }
}

/// The caller's id of an anchor: never hashed, only numbered.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AnchorId(pub u64);

/// The caller's id of a syntax context: never hashed, only numbered.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct CtxId(pub u32);

/// A span as the caller holds it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct InSpan {
    pub anchor: AnchorId,
    pub range: RelativeRange,
    pub ctx: CtxId,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Delimiter {
    Parenthesis,
    Brace,
    Bracket,
    Invisible,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LitKind {
    Byte,
    Char,
    Integer,
    Float,
    Str,
    /// A raw string with this many `#`.
    StrRaw(u8),
    ByteStr,
    ByteStrRaw(u8),
    CStr,
    CStrRaw(u8),
    Err,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TokenKind {
    Ident,
    RawIdent,
    Lifetime,
    /// One punctuation character; `joint` when the next token is glued to it.
    Punct { joint: bool },
    Literal(LitKind),
    Open(Delimiter),
    Close(Delimiter),
}

impl TokenKind {
    fn tag(self) -> [u8; 2] {
        let delim = |d: Delimiter| match d {
            Delimiter::Parenthesis => 0,
            Delimiter::Brace => 1,
            Delimiter::Bracket => 2,
            Delimiter::Invisible => 3,
        };
        match self {
            TokenKind::Ident => [0, 0],
            TokenKind::RawIdent => [1, 0],
            TokenKind::Lifetime => [2, 0],
            TokenKind::Punct { joint } => [3, joint as u8],
            TokenKind::Literal(lit) => match lit {
                LitKind::Byte => [4, 0],
                LitKind::Char => [5, 0],
                LitKind::Integer => [6, 0],
                LitKind::Float => [7, 0],
                LitKind::Str => [8, 0],
                LitKind::StrRaw(n) => [9, n],
                LitKind::ByteStr => [10, 0],
                LitKind::ByteStrRaw(n) => [11, n],
                LitKind::CStr => [12, 0],
                LitKind::CStrRaw(n) => [13, n],
                LitKind::Err => [14, 0],
            },
            TokenKind::Open(d) => [15, delim(d)],
            TokenKind::Close(d) => [16, delim(d)],
        }
    }

    fn of_tag(tag: [u8; 2]) -> Option<TokenKind> {
        let delim = |d: u8| match d {
            0 => Some(Delimiter::Parenthesis),
            1 => Some(Delimiter::Brace),
            2 => Some(Delimiter::Bracket),
            3 => Some(Delimiter::Invisible),
            _ => None,
        };
        let [kind, arg] = tag;
        Some(match kind {
            0 => TokenKind::Ident,
            1 => TokenKind::RawIdent,
            2 => TokenKind::Lifetime,
            3 => TokenKind::Punct { joint: arg != 0 },
            4 => TokenKind::Literal(LitKind::Byte),
            5 => TokenKind::Literal(LitKind::Char),
            6 => TokenKind::Literal(LitKind::Integer),
            7 => TokenKind::Literal(LitKind::Float),
            8 => TokenKind::Literal(LitKind::Str),
            9 => TokenKind::Literal(LitKind::StrRaw(arg)),
            10 => TokenKind::Literal(LitKind::ByteStr),
            11 => TokenKind::Literal(LitKind::ByteStrRaw(arg)),
            12 => TokenKind::Literal(LitKind::CStr),
            13 => TokenKind::Literal(LitKind::CStrRaw(arg)),
            14 => TokenKind::Literal(LitKind::Err),
            15 => TokenKind::Open(delim(arg)?),
            16 => TokenKind::Close(delim(arg)?),
            _ => return None,
        })
    }
}

/// One token of a stream, flattened: a group is its `Open`, its tokens and its `Close`, each
/// delimiter with its own span. `text` is what the token spells (a literal's symbol and
/// suffix together; empty for a delimiter).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Token<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    pub span: InSpan,
}

/// The anchors and contexts of a call, by ordinal of first appearance: the def, call and
/// mixed sites first (when the stream has them), then the input in order.
#[derive(Clone, Debug)]
pub struct Numbering {
    anchors: Vec<AnchorId, Global>,
    ctxs: Vec<CtxId, Global>,
}

impl Default for Numbering {
    fn default() -> Numbering {
        Numbering { anchors: Vec::new_in(Global), ctxs: Vec::new_in(Global) }
    }
}

impl Numbering {
    /// The ordinal of `anchor`, numbering it if it is new. A call has few anchors and
    /// contexts (one or two of each, a derive's input a handful), so a scan beats a map.
    fn number_anchor(&mut self, anchor: AnchorId) -> u32 {
        match self.anchors.iter().position(|&it| it == anchor) {
            Some(at) => at as u32,
            None => {
                self.anchors.push(anchor);
                (self.anchors.len() - 1) as u32
            }
        }
    }

    fn number_ctx(&mut self, ctx: CtxId) -> u32 {
        match self.ctxs.iter().position(|&it| it == ctx) {
            Some(at) => at as u32,
            None => {
                self.ctxs.push(ctx);
                (self.ctxs.len() - 1) as u32
            }
        }
    }

    pub fn anchor_ordinal(&self, anchor: AnchorId) -> Option<u32> {
        self.anchors.iter().position(|&it| it == anchor).map(|at| at as u32)
    }

    pub fn ctx_ordinal(&self, ctx: CtxId) -> Option<u32> {
        self.ctxs.iter().position(|&it| it == ctx).map(|at| at as u32)
    }

    pub fn anchor(&self, ordinal: u32) -> Option<AnchorId> {
        self.anchors.get(ordinal as usize).copied()
    }

    pub fn ctx(&self, ordinal: u32) -> Option<CtxId> {
        self.ctxs.get(ordinal as usize).copied()
    }

    pub fn ctx_count(&self) -> u32 {
        self.ctxs.len() as u32
    }
}

/// What a stream is, in its hash's domain: one stream never hashes as another's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    /// A macro call's input (with the call's three sites).
    Input,
    /// An attribute macro's own arguments.
    Attr,
    /// A declarative macro's definition.
    Definition,
}

const STREAM_DOMAIN: &str = "blaze-expansion token stream 1";

/// Hashes a stream into its position-free digest, numbering as it goes.
pub struct TokenHasher {
    hasher: Hasher,
    numbering: Numbering,
    count: u64,
    #[cfg(test)]
    pub(crate) plant: TokenPlant,
}

/// A planted bug of the token hash, for its tests' controls.
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum TokenPlant {
    #[default]
    None,
    /// The anchor's caller id is hashed in place of its ordinal: another numbering of the same
    /// stream hashes otherwise.
    HashAnchorIds,
}

impl TokenHasher {
    /// A hasher of a stream of `role`, numbering on from `numbering` (an attribute's arguments
    /// are numbered after the input, so one table resolves both).
    pub fn new(role: Role, numbering: Numbering) -> TokenHasher {
        let mut hasher = Hasher::new(STREAM_DOMAIN);
        hasher.update(&[match role {
            Role::Input => 0,
            Role::Attr => 1,
            Role::Definition => 2,
        }]);
        TokenHasher {
            hasher,
            numbering,
            count: 0,
            #[cfg(test)]
            plant: TokenPlant::None,
        }
    }

    fn span(&mut self, span: InSpan) {
        let anchor = self.numbering.number_anchor(span.anchor);
        #[cfg(test)]
        if self.plant == TokenPlant::HashAnchorIds {
            self.hasher.update(&span.anchor.0.to_le_bytes());
        }
        let ctx = self.numbering.number_ctx(span.ctx);
        self.hasher
            .update(&anchor.to_le_bytes())
            .update(&span.range.start.to_le_bytes())
            .update(&span.range.end.to_le_bytes())
            .update(&ctx.to_le_bytes());
    }

    /// The call's def, call and mixed sites: hashed first, so they have the first ordinals.
    pub fn sites(&mut self, sites: [InSpan; 3]) -> &mut TokenHasher {
        self.hasher.update(b"s");
        for site in sites {
            self.span(site);
        }
        self
    }

    pub fn token(&mut self, token: &Token<'_>) -> &mut TokenHasher {
        self.hasher.update(b"t").update(&token.kind.tag()).part(token.text.as_bytes());
        self.span(token.span);
        self.count += 1;
        self
    }

    pub fn finish(mut self) -> (Digest, Numbering) {
        self.hasher.update(b"e").update(&self.count.to_le_bytes());
        (self.hasher.finish(), self.numbering)
    }
}

/// The digest of `tokens` of `role`, with `sites` first when given, and its numbering.
pub fn stream_digest<'a>(
    role: Role,
    sites: Option<[InSpan; 3]>,
    tokens: impl IntoIterator<Item = Token<'a>>,
) -> (Digest, Numbering) {
    let mut hasher = TokenHasher::new(role, Numbering::default());
    if let Some(sites) = sites {
        hasher.sites(sites);
    }
    for token in tokens {
        hasher.token(&token);
    }
    hasher.finish()
}

/// A declarative macro's transparency (`macro_rules!` is semi-opaque, `macro` opaque, a
/// `#[rustc_macro_transparency]` may say otherwise): what its expansion's marks are.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Transparency {
    Transparent,
    SemiOpaque,
    Opaque,
}

impl Transparency {
    fn byte(self) -> u8 {
        match self {
            Transparency::Transparent => 0,
            Transparency::SemiOpaque => 1,
            Transparency::Opaque => 2,
        }
    }

    fn of(byte: u8) -> Option<Transparency> {
        match byte {
            0 => Some(Transparency::Transparent),
            1 => Some(Transparency::SemiOpaque),
            2 => Some(Transparency::Opaque),
            _ => None,
        }
    }
}

/// A context an expansion made: the call's mark of `transparency` applied over the context of
/// ordinal `over` (an input context, or an earlier mark). Its own ordinal is the call's
/// context count plus its index among the marks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mark {
    pub over: u32,
    pub transparency: Transparency,
}

/// A span of an expansion's output: an anchor ordinal of the call's numbering, a range from
/// that anchor, and a context ordinal (an input's, or a [`Mark`]'s past them).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutSpan {
    pub anchor: u32,
    pub range: RelativeRange,
    pub ctx: u32,
}

/// One token of an output, as read from its record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OutToken<'a> {
    pub kind: TokenKind,
    pub text: &'a str,
    pub span: OutSpan,
}

/// Why an output can't be written against its call's numbering.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Unnumbered {
    /// An output span anchored to an item the call's input and sites never named.
    Anchor(AnchorId),
    /// An output span of a context the input never had and no mark made.
    Ctx(CtxId),
}

impl fmt::Display for Unnumbered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unnumbered::Anchor(anchor) => write!(
                f,
                "its output has a span anchored to {anchor:?}, which neither its input nor its \
                 sites name: an expansion's output is written against its call alone (rule 71)"
            ),
            Unnumbered::Ctx(ctx) => write!(
                f,
                "its output has a span of the context {ctx:?}, which its input has not and no mark \
                 of the expansion made"
            ),
        }
    }
}

/// Writes an output's tokens into a value, against the call's numbering.
pub struct OutputWriter<'n> {
    numbering: &'n Numbering,
    /// The marks made so far, each with the caller's id of the context it is.
    marks: Vec<(CtxId, Mark), Global>,
    tokens: Vec<u8, Global>,
    count: u32,
}

impl<'n> OutputWriter<'n> {
    pub fn new(numbering: &'n Numbering) -> OutputWriter<'n> {
        OutputWriter { numbering, marks: Vec::new_in(Global), tokens: Vec::new_in(Global), count: 0 }
    }

    /// Notes that the caller's context `ctx` is a mark of the expansion over `over` (a context
    /// the input has or an earlier mark).
    pub fn mark(&mut self, ctx: CtxId, over: CtxId, transparency: Transparency) -> Result<(), Unnumbered> {
        let over = self.ctx(over)?;
        self.marks.push((ctx, Mark { over, transparency }));
        Ok(())
    }

    fn ctx(&self, ctx: CtxId) -> Result<u32, Unnumbered> {
        if let Some(ordinal) = self.numbering.ctx_ordinal(ctx) {
            return Ok(ordinal);
        }
        match self.marks.iter().position(|it| it.0 == ctx) {
            Some(at) => Ok(self.numbering.ctx_count() + at as u32),
            None => Err(Unnumbered::Ctx(ctx)),
        }
    }

    pub fn token(&mut self, token: &Token<'_>) -> Result<(), Unnumbered> {
        let anchor = self
            .numbering
            .anchor_ordinal(token.span.anchor)
            .ok_or(Unnumbered::Anchor(token.span.anchor))?;
        let ctx = self.ctx(token.span.ctx)?;
        self.tokens.extend_from_slice(&token.kind.tag());
        let len = u32::try_from(token.text.len()).expect("a token is under 4 GiB");
        self.tokens.extend_from_slice(&len.to_le_bytes());
        self.tokens.extend_from_slice(token.text.as_bytes());
        for word in [anchor, token.span.range.start, token.span.range.end, ctx] {
            self.tokens.extend_from_slice(&word.to_le_bytes());
        }
        self.count += 1;
        Ok(())
    }

    /// Writes the marks and tokens into `w`.
    pub(crate) fn write(&self, w: &mut Writer) {
        w.u32(self.numbering.ctx_count());
        w.u32(self.marks.len() as u32);
        for (_, mark) in &self.marks {
            w.u32(mark.over).u8(mark.transparency.byte());
        }
        w.u32(self.count);
        w.bytes(&self.tokens);
    }
}

/// An output as its record holds it, read in place.
#[derive(Clone)]
pub struct OutputRef<'a> {
    /// The call's context count: a context ordinal past it is a mark.
    pub ctxs: u32,
    marks: &'a [u8],
    mark_count: u32,
    tokens: &'a [u8],
    pub count: u32,
}

impl<'a> OutputRef<'a> {
    pub(crate) fn read(r: &mut Reader<'a>) -> Result<OutputRef<'a>, Corrupt> {
        let ctxs = r.u32()?;
        let mark_count = r.u32()?;
        // Each mark is five bytes: read them as one slice, checked below.
        let mut marks_reader = r.clone();
        for _ in 0..mark_count {
            marks_reader.u32()?;
            let byte = marks_reader.u8()?;
            Transparency::of(byte).ok_or(marks_reader.corrupt(Why::Tag(byte)))?;
        }
        let marks = r.between(&marks_reader);
        *r = marks_reader;
        let count = r.u32()?;
        let tokens = r.bytes()?;
        Ok(OutputRef { ctxs, marks, mark_count, tokens, count })
    }

    pub fn marks(&self) -> impl Iterator<Item = Mark> + 'a {
        let marks = self.marks;
        (0..self.mark_count as usize).map(move |i| {
            let at = i * 5;
            let over = u32::from_le_bytes(marks[at..at + 4].try_into().expect("checked at read"));
            let transparency = Transparency::of(marks[at + 4]).expect("checked at read");
            Mark { over, transparency }
        })
    }

    /// The tokens, each read in place; a token that does not read is an error naming where.
    pub fn tokens(&self) -> OutTokens<'a> {
        OutTokens { bytes: self.tokens, at: 0, left: self.count }
    }
}

/// The tokens of an [`OutputRef`].
pub struct OutTokens<'a> {
    bytes: &'a [u8],
    at: usize,
    left: u32,
}

impl<'a> Iterator for OutTokens<'a> {
    type Item = Result<OutToken<'a>, Corrupt>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let corrupt = |at, why| Corrupt { kind: "expansion output", at, why };
        let bytes = self.bytes;
        let at = self.at;
        let Some(head) = bytes.get(at..at + 6) else {
            self.left = 0;
            return Some(Err(corrupt(at, Why::Short)));
        };
        let Some(kind) = TokenKind::of_tag([head[0], head[1]]) else {
            self.left = 0;
            return Some(Err(corrupt(at, Why::Tag(head[0]))));
        };
        let len = u32::from_le_bytes(head[2..6].try_into().expect("six bytes")) as usize;
        let text_at = at + 6;
        let Some(text) = bytes.get(text_at..text_at + len) else {
            self.left = 0;
            return Some(Err(corrupt(text_at, Why::Short)));
        };
        let Ok(text) = std::str::from_utf8(text) else {
            self.left = 0;
            return Some(Err(corrupt(text_at, Why::Utf8)));
        };
        let words_at = text_at + len;
        let Some(words) = bytes.get(words_at..words_at + 16) else {
            self.left = 0;
            return Some(Err(corrupt(words_at, Why::Short)));
        };
        let word = |i: usize| u32::from_le_bytes(words[i * 4..i * 4 + 4].try_into().expect("16 bytes"));
        self.at = words_at + 16;
        Some(Ok(OutToken {
            kind,
            text,
            span: OutSpan {
                anchor: word(0),
                range: RelativeRange { start: word(1), end: word(2) },
                ctx: word(3),
            },
        }))
    }
}

/// A declarative macro's code identity: its definition's tokens hashed position-free (no
/// sites: a definition is not a call), its transparency, its crate's edition, and the digest
/// of the defining crate's cfg where the definition's text names `cfg` (a `cfg_attr` deciding
/// its transparency; hir-expand's `stored::cfg_part`), `None` otherwise.
pub fn definition_identity<'a>(
    tokens: impl IntoIterator<Item = Token<'a>>,
    transparency: Transparency,
    edition: u16,
    cfg: Option<Digest>,
) -> Digest {
    let (tokens, _) = stream_digest(Role::Definition, None, tokens);
    let mut hasher = Hasher::new("blaze-expansion declarative definition 1");
    hasher
        .id(&tokens)
        .update(&[transparency.byte()])
        .update(&edition.to_le_bytes())
        .opt_part(cfg.as_ref().map(|it| it.0));
    hasher.finish()
}
