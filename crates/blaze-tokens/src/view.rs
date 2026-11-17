//! [`Tokens`]: a token record read in place, checked once by [`Tokens::open`]; a sub-view of a
//! range ([`Tokens::slice`]) shares the record's lexeme and context tables. The token digest
//! ([`Tokens::digest`]) and the one rendering for a person ([`Tokens::shown`]) are here.

use core::fmt;

use blaze_format_core::{FormatError, open_as};
use blaze_id::ContentId;
use blaze_ids::Hasher;
use blaze_surface::Edition;
use parser::SyntaxKind;

use crate::build::in_order;
use crate::form::{
    Ctx, CtxNode, Lexeme, MAX_TOKENS, Mark, MarkOf, NO_LEXEME, NodeRow, Origin, PAD,
    TOKENS_SCHEMA, TokenRange, TokensRoot, at, check_word, lexeme_span, needs_lexeme, wide,
};

/// The domain of [`Tokens::digest`].
pub const DIGEST_DOMAIN: &str = "blaze-tokens range digest 1";

/// A column of the record, as an [`OpenError`] names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Column {
    Words,
    Lexemes,
    Contexts,
    OriginValues,
    OriginTags,
    Joined,
    LexemeEnds,
    LexemeBytes,
    Nodes,
    Groups,
}

/// Why bytes are not a token record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum OpenError {
    /// Not a buffer of [`crate::TOKENS_SCHEMA`], or cut short, or its root does not check:
    /// blaze-format's check of the layout refused it.
    Format(FormatError),
    /// `column` has `found` entries where the token count gives `wanted`.
    Length { column: Column, found: u64, wanted: u64 },
    /// Entry `row` of `column` is not one a [`crate::TokensBuilder`] writes: `what` says why.
    Invalid { column: Column, row: u64, what: &'static str },
    /// Token `token` names lexeme `index` of a table of `lexemes`.
    LexemeOutOfRange { token: u32, index: u32, lexemes: u32 },
    /// Row `row` of `column` is a token range ending at `end`, past the record's `tokens`.
    RangePastEnd { column: Column, row: u32, end: u32, tokens: u32 },
}

impl From<FormatError> for OpenError {
    fn from(error: FormatError) -> OpenError {
        OpenError::Format(error)
    }
}

impl fmt::Display for OpenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpenError::Format(error) => write!(f, "a token record: {error}"),
            OpenError::Length { column, found, wanted } => write!(
                f,
                "a token record's {column:?} column has {found} entries where its tokens give {wanted}"
            ),
            OpenError::Invalid { column, row, what } => {
                write!(f, "a token record's {column:?} column, entry {row}: {what}")
            }
            OpenError::LexemeOutOfRange { token, index, lexemes } => write!(
                f,
                "a token record's token {token} names lexeme {index} of a table of {lexemes}"
            ),
            OpenError::RangePastEnd { column, row, end, tokens } => write!(
                f,
                "a token record's {column:?} column, entry {row}: a range ending at token {end}, past its {tokens} tokens"
            ),
        }
    }
}

impl core::error::Error for OpenError {}

/// A token record, or a range of one, read in place. Everything [`Tokens::open`] checked holds
/// for every view of the record, so no accessor checks a column again; an index past the
/// view's tokens panics, as a slice's does.
///
/// Indexes are the view's own: token 0 is the first of its range, so a nested call's input is
/// a view whose token `n` is the call's input token `n` ([`Origin::Input`]); [`Tokens::offset`]
/// is where the view starts in its record.
#[derive(Clone, Copy, Debug)]
pub struct Tokens<'a> {
    /// The view's words, then every word to the record's end and its pad.
    padded: &'a [u32],
    len: usize,
    lexeme_of: &'a [u32],
    ctxs: &'a [u32],
    origin_values: &'a [u32],
    origin_tags: &'a [u8],
    /// The record's whole joined column: bit `offset + i` is token `i`'s.
    joined: &'a [u64],
    lexeme_ends: &'a [u32],
    lexeme_bytes: &'a str,
    nodes: &'a [NodeRow],
    /// The record's whole groups column.
    groups: &'a [TokenRange],
    offset: usize,
}

impl<'a> Tokens<'a> {
    /// The record in `buffer`, every column checked once: the layout by blaze-format, then
    /// each column's length against the token count, each word a word the parser takes, each
    /// lexeme present exactly for the kinds with no fixed text, numbered in first-seen order
    /// and inside the table, each context in the table and each parent before its child, each
    /// origin one [`Origin`] writes, no joined bit past the last token, each group inside the
    /// tokens and in order. A buffer that fails is refused with the column and entry.
    pub fn open(buffer: &'a [u8]) -> Result<Tokens<'a>, OpenError> {
        let root = open_as::<TokensRoot>(buffer, TOKENS_SCHEMA)?;
        let padded = root.words.follow(buffer)?;
        let lexeme_of = root.lexeme_of.follow(buffer)?;
        let ctxs = root.ctxs.follow(buffer)?;
        let origin_values = root.origin_values.follow(buffer)?;
        let origin_tags = root.origin_tags.follow(buffer)?;
        let joined = root.joined.follow(buffer)?;
        let lexeme_ends = root.lexeme_ends.follow(buffer)?;
        let lexeme_bytes = root.lexeme_bytes.as_str(buffer)?;
        let nodes = root.nodes.follow(buffer)?;
        let groups = root.groups.follow(buffer)?;

        let n = lexeme_of.len();
        let tokens = u32::try_from(n).ok().filter(|&n| n < MAX_TOKENS).ok_or(OpenError::Length {
            column: Column::Lexemes,
            found: wide(n),
            wanted: u64::from(MAX_TOKENS),
        })?;
        let length = |column: Column, found: usize, wanted: usize| match found == wanted {
            true => Ok(()),
            false => Err(OpenError::Length { column, found: wide(found), wanted: wide(wanted) }),
        };
        length(Column::Words, padded.len(), n + PAD)?;
        length(Column::Contexts, ctxs.len(), n)?;
        length(Column::OriginValues, origin_values.len(), n)?;
        length(Column::OriginTags, origin_tags.len(), n)?;
        length(Column::Joined, joined.len(), n.div_ceil(64))?;
        if padded[n..].iter().any(|&word| word != parser::Tokens::EOF_WORD) {
            return Err(OpenError::Invalid {
                column: Column::Words,
                row: wide(n),
                what: "words that do not end in the parser's 4 EOF words",
            });
        }

        let mut end = 0usize;
        for (k, &next) in lexeme_ends.iter().enumerate() {
            let next = at(next);
            let what = if next < end {
                Some("a lexeme that ends before the one before it")
            } else if next > lexeme_bytes.len() {
                Some("a lexeme past the lexeme bytes")
            } else if !lexeme_bytes.is_char_boundary(next) {
                Some("a lexeme cut inside a character")
            } else {
                None
            };
            if let Some(what) = what {
                return Err(OpenError::Invalid { column: Column::LexemeEnds, row: wide(k), what });
            }
            end = next;
        }
        if end != lexeme_bytes.len() {
            return Err(OpenError::Invalid {
                column: Column::LexemeBytes,
                row: wide(end),
                what: "lexeme bytes after the last lexeme",
            });
        }
        let lexemes = u32::try_from(lexeme_ends.len())
            .ok()
            .filter(|&count| count < NO_LEXEME)
            .ok_or(OpenError::Length {
                column: Column::LexemeEnds,
                found: wide(lexeme_ends.len()),
                wanted: u64::from(NO_LEXEME),
            })?;

        for (j, node) in nodes.iter().enumerate() {
            if let Some(what) = node.refused(j) {
                return Err(OpenError::Invalid { column: Column::Nodes, row: wide(j), what });
            }
        }

        // The next lexeme number a token may introduce: first-seen order.
        let mut fresh = 0u32;
        for i in 0..n {
            let row = wide(i);
            let token = u32::try_from(i).expect("below MAX_TOKENS");
            let kind = check_word(padded[i])
                .map_err(|what| OpenError::Invalid { column: Column::Words, row, what })?;
            let index = lexeme_of[i];
            match (needs_lexeme(kind), index) {
                (true, NO_LEXEME) => {
                    return Err(OpenError::Invalid {
                        column: Column::Lexemes,
                        row,
                        what: "no lexeme for a token whose kind's text is not fixed",
                    });
                }
                (true, index) if index >= lexemes => {
                    return Err(OpenError::LexemeOutOfRange { token, index, lexemes });
                }
                (true, index) if index > fresh => {
                    return Err(OpenError::Invalid {
                        column: Column::Lexemes,
                        row,
                        what: "a lexeme numbered before its first use: lexemes are in first-seen order",
                    });
                }
                (true, index) => fresh = fresh.max(index + 1),
                (false, NO_LEXEME) => {}
                (false, _) => {
                    return Err(OpenError::Invalid {
                        column: Column::Lexemes,
                        row,
                        what: "a lexeme for a token whose kind's text is fixed",
                    });
                }
            }
            if !Ctx::is_in(ctxs[i], nodes.len()) {
                return Err(OpenError::Invalid {
                    column: Column::Contexts,
                    row,
                    what: "a context the record's table does not have",
                });
            }
            if Origin::from_parts(origin_tags[i], origin_values[i]).is_none() {
                return Err(OpenError::Invalid {
                    column: Column::OriginTags,
                    row,
                    what: "an origin of no kind, or made with no transparency",
                });
            }
        }
        if fresh != lexemes {
            return Err(OpenError::Invalid {
                column: Column::LexemeEnds,
                row: u64::from(fresh),
                what: "a lexeme no token uses",
            });
        }
        if n % 64 != 0 && joined[n / 64] >> (n % 64) != 0 {
            return Err(OpenError::Invalid {
                column: Column::Joined,
                row: wide(n / 64),
                what: "a joined bit past the last token",
            });
        }

        let mut last: Option<TokenRange> = None;
        for (j, &group) in groups.iter().enumerate() {
            let row = u32::try_from(j).map_err(|_| OpenError::Length {
                column: Column::Groups,
                found: wide(groups.len()),
                wanted: u64::from(u32::MAX),
            })?;
            if group.end < group.start {
                return Err(OpenError::Invalid {
                    column: Column::Groups,
                    row: u64::from(row),
                    what: "a range that ends before it starts",
                });
            }
            if group.end > tokens {
                return Err(OpenError::RangePastEnd {
                    column: Column::Groups,
                    row,
                    end: group.end,
                    tokens,
                });
            }
            if last.is_some_and(|last| !in_order(last, group)) {
                return Err(OpenError::Invalid {
                    column: Column::Groups,
                    row: u64::from(row),
                    what: "a group before one it follows: groups are sorted by start, outer first",
                });
            }
            last = Some(group);
        }

        Ok(Tokens {
            padded,
            len: n,
            lexeme_of,
            ctxs,
            origin_values,
            origin_tags,
            joined,
            lexeme_ends,
            lexeme_bytes,
            nodes,
            groups,
            offset: 0,
        })
    }

    /// How many tokens the view has.
    pub fn len(self) -> usize {
        self.len
    }

    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Every token of the view, as a range of it.
    pub fn all(self) -> TokenRange {
        TokenRange::new(0, u32::try_from(self.len).expect("a record's tokens fit a u32"))
    }

    /// Where the view starts in its record: token `i` of the view is token `offset + i` there.
    pub fn offset(self) -> u32 {
        u32::try_from(self.offset).expect("a record's tokens fit a u32")
    }

    /// The tokens of `range`, a range of this view, as a view of their own sharing the
    /// record's tables: a nested call's input in its parent's tokens (D122 rule 4). Panics on
    /// a range past the view.
    pub fn slice(self, range: TokenRange) -> Tokens<'a> {
        let (start, end) = self.bounds(range);
        Tokens {
            padded: &self.padded[start..],
            len: end - start,
            lexeme_of: &self.lexeme_of[start..end],
            ctxs: &self.ctxs[start..end],
            origin_values: &self.origin_values[start..end],
            origin_tags: &self.origin_tags[start..end],
            offset: self.offset + start,
            ..self
        }
    }

    /// The parser's words of `range`, one per token, with no pad: what
    /// `parser::Input::extend_from_words` appends, or compares.
    pub fn words(self, range: TokenRange) -> &'a [u32] {
        let (start, end) = self.bounds(range);
        &self.padded[start..end]
    }

    /// The whole view as the parser's input, with no copy: `Some` when the view ends where its
    /// record does, whose words are followed by the pad `parser::Tokens::new` asks for. Every
    /// fragment start is counted; a view that starts inside a group takes
    /// `without_first_fragment`, an expansion's output `ignoring_fragments`. For a view that
    /// ends before its record, [`Tokens::copy_words_into`].
    pub fn parser_tokens(self) -> Option<parser::Tokens<'a>> {
        match self.padded.len() == self.len + PAD {
            true => Some(parser::Tokens::new(self.padded, true)),
            false => None,
        }
    }

    /// Appends the view's words to `input`: one copy of 4 bytes a token, nothing converted or
    /// lexed. The parser reads a window that ends before its record only from its own buffer,
    /// which must end in the pad.
    pub fn copy_words_into(self, input: &mut parser::Input) {
        input.extend_from_words(&self.padded[..self.len]);
    }

    /// The parser's word of token `i`.
    pub fn word(self, i: usize) -> u32 {
        self.padded[..self.len][i]
    }

    /// The kind of token `i`.
    pub fn kind(self, i: usize) -> SyntaxKind {
        parser::word_kind(self.word(i))
    }

    /// Whether token `i` is written right against the next token.
    pub fn is_joined(self, i: usize) -> bool {
        assert!(i < self.len, "token {i} of a view of {} tokens", self.len);
        let bit = self.offset + i;
        (self.joined[bit / 64] >> (bit % 64)) & 1 == 1
    }

    /// The lexeme of token `i`; `None` for a kind whose text is fixed.
    pub fn lexeme_of(self, i: usize) -> Option<Lexeme> {
        match self.lexeme_of[i] {
            NO_LEXEME => None,
            k => Some(Lexeme(k)),
        }
    }

    /// How many distinct lexemes the record holds.
    pub fn lexeme_count(self) -> usize {
        self.lexeme_ends.len()
    }

    /// The text of `lexeme`, a lexeme of this record.
    pub fn lexeme_text(self, lexeme: Lexeme) -> &'a str {
        &self.lexeme_bytes[lexeme_span(self.lexeme_ends, at(lexeme.0))]
    }

    /// The lexeme bytes of token `i`: its text as a lexer reads it from source (an identifier
    /// with its `r#`, a lifetime with its `'`, a literal whole); `None` for a kind whose text
    /// is fixed.
    pub fn lexeme(self, i: usize) -> Option<&'a str> {
        self.lexeme_of(i).map(|lexeme| self.lexeme_text(lexeme))
    }

    /// The text of token `i`: its lexeme, or its kind's fixed text.
    pub fn text(self, i: usize) -> &'a str {
        match self.lexeme(i) {
            Some(text) => text,
            None => self
                .kind(i)
                .fixed_text()
                .expect("a token with no lexeme is of a kind with a fixed text (checked at open)"),
        }
    }

    /// The hygiene context of token `i`.
    pub fn ctx(self, i: usize) -> Ctx {
        Ctx::from_raw(self.ctxs[i])
    }

    /// The node `ctx` names; `None` for a root.
    pub fn node(self, ctx: Ctx) -> Option<CtxNode> {
        ctx.node().map(|index| self.nodes[at(index)].node())
    }

    /// The record's context table, in order.
    pub fn nodes(self) -> impl Iterator<Item = CtxNode> + use<'a> {
        self.nodes.iter().map(|row| row.node())
    }

    /// The innermost mark of token `i`: `None` for a token no expansion made, which resolves
    /// where the reader places the record. A path is resolved by its first segment's.
    pub fn mark(self, i: usize) -> Option<Mark> {
        self.node(self.ctx(i)).map(|node| node.mark)
    }

    /// The edition token `i` is read in: its root's, or its innermost mark's.
    pub fn edition(self, i: usize) -> Edition {
        let ctx = self.ctx(i);
        match self.node(ctx) {
            Some(node) => node.mark.edition,
            None => ctx.root_edition().expect("a root context is of an edition (checked at open)"),
        }
    }

    /// Where token `i` came from.
    pub fn origin(self, i: usize) -> Origin {
        Origin::from_parts(self.origin_tags[i], self.origin_values[i])
            .expect("an origin's parts are one `Origin` writes (checked at open)")
    }

    /// The invisible groups wholly inside the view, as ranges of it, outer first.
    pub fn groups(self) -> impl Iterator<Item = TokenRange> + use<'a> {
        let start = self.offset;
        let end = self.offset + self.len;
        let first = self.groups.partition_point(|group| at(group.start) < start);
        self.groups[first..]
            .iter()
            .take_while(move |group| at(group.start) <= end)
            .filter(move |group| at(group.end) <= end)
            .map(move |group| {
                let base = u32::try_from(start).expect("a record's tokens fit a u32");
                TokenRange::new(group.start - base, group.end - base)
            })
    }

    /// The digest of the tokens of `range`: the one digest of expansion-made things (D122 rule
    /// 6), equal for equal tokens wherever they are and however their record numbers its
    /// lexemes and contexts.
    ///
    /// Hashed, in domain [`DIGEST_DOMAIN`]: the token count as a little-endian u64; then per
    /// token in order: its parser word as 4 little-endian bytes (kind, contextual kind, edition,
    /// the parser's joint, fragment and extension bits), its text ([`Tokens::text`]) as a
    /// length-prefixed part, one byte for `joined` (0 for the range's last token, whose next
    /// token is not in the range), then its context's chain of marks innermost first (per
    /// mark: 1, the mark of: 0 for this expansion or 1 and its key, transparency byte, edition
    /// byte, crate key, 0 or 1 and the module key) ended by 0 and the root's edition byte;
    /// then per invisible group wholly inside the range, outer first, 1 and its start and end
    /// relative to the range as little-endian u32s, ended by 0. Not hashed: lexeme or context
    /// numbers, the range's offset, origins (where a token came from is not what it is).
    pub fn digest(self, range: TokenRange) -> ContentId {
        let (start, end) = self.bounds(range);
        let mut h = Hasher::new(DIGEST_DOMAIN);
        h.update(&wide(end - start).to_le_bytes());
        for i in start..end {
            h.update(&self.word(i).to_le_bytes());
            h.part(self.text(i).as_bytes());
            h.update(&[u8::from(i + 1 < end && self.is_joined(i))]);
            let mut ctx = self.ctx(i);
            while let Some(node) = self.node(ctx) {
                h.update(&[1]);
                hash_mark(&mut h, node.mark);
                ctx = node.parent;
            }
            let root = ctx.root_edition().expect("a chain of contexts ends in a root (checked at open)");
            h.update(&[0, root.byte()]);
        }
        let inside = self.slice(range);
        for group in inside.groups() {
            h.update(&[1]);
            h.update(&group.start.to_le_bytes());
            h.update(&group.end.to_le_bytes());
        }
        h.update(&[0]);
        h.finish()
    }

    /// The tokens of `range` as text for a person: a diagnostic, `blaze explain`. NOTHING may
    /// lex this output: it is no source, and a reader of an expansion reads its tokens (D122
    /// rule 7). A space goes between two tokens unless the first is joined to the second, and
    /// always between an integer and a `.` after it, so `999` `.` `try_into` reads as no float
    /// to a person either: `999 .try_into()` when the `.` is joined, `999 . try_into()` when not.
    pub fn shown(self, range: TokenRange, out: &mut impl fmt::Write) -> fmt::Result {
        let (start, end) = self.bounds(range);
        for i in start..end {
            if i > start {
                let before = i - 1;
                let integer_dot = self.kind(before) == SyntaxKind::INT_NUMBER
                    && self.kind(i) == SyntaxKind::DOT;
                if !self.is_joined(before) || integer_dot {
                    out.write_char(' ')?;
                }
            }
            out.write_str(self.text(i))?;
        }
        Ok(())
    }

    /// `range` as indexes of this view. Panics on a range past the view or ending before it
    /// starts.
    fn bounds(self, range: TokenRange) -> (usize, usize) {
        let (start, end) = (at(range.start), at(range.end));
        assert!(
            start <= end && end <= self.len,
            "tokens {start}..{end} of a view of {} tokens",
            self.len
        );
        (start, end)
    }
}

fn hash_mark(h: &mut Hasher, mark: Mark) {
    match mark.of {
        MarkOf::This => h.update(&[0]),
        MarkOf::Outer(key) => h.update(&[1]).id(&key),
    };
    h.update(&[mark.transparency.byte(), mark.edition.byte()]);
    h.id(&mark.site.krate.0);
    match mark.site.module {
        None => h.update(&[0]),
        Some(module) => h.update(&[1]).id(&module.0),
    };
}
