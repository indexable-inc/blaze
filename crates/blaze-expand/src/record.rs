//! An [`Expansion`] as a stored record, and read back: one blaze-format buffer of columns
//! (docs/v2/plan/design/bottom.md, "Layout: columns"), written once and read in one pass.
//!
//! The root is one [`Slice`] of bytes per [`RecordColumn`]. A token is a packed 4-byte entry of its
//! kind ([`RecordColumn::Tokens`]); its text is in one side table ([`RecordColumn::Text`]), in token order,
//! cut by the LEB128 lengths of [`RecordColumn::TextLens`]; its context and its origin are LEB128
//! columns of their own, a literal's suffix length another. Every count and index is LEB128;
//! a key is its 32 bytes. Nothing is a byte offset into a file (rule 71), and the bytes are a
//! function of the value (rule 25): equal expansions write equal buffers.
//!
//! [`Expansion::read_record`] checks everything once, so the value it gives holds what
//! [`Expansion`] promises (a token's context is its stream's, a hole and a nested call name
//! tokens it has, a diagnostic's text is a whole `str` of its notes); a buffer that is not one
//! this writes is a typed [`RecordError`] naming the column and the byte.

use core::fmt;
use core::mem::size_of;

use blaze_own::{AVec, Alloc, Vec};

use blaze_format::{FormatError, Part, Record, SchemaHash, Slice, Writer, WriterOpts, leb};
use blaze_id::{ContentId, CrateItemKey, ItemKey};
use blaze_tokens::{OpenError, Tokens};

use crate::{
    error::StreamError,
    token_record::{OriginMaps, TokenRecordError},
    expansion::{
        Diagnostic, Expansion, Hole, HoleOf, IncludeFailure, NestedCall, NestedKind, TokenOrigin,
        PathScope, ProcLevel, Range, ReadAnswer, ReadKind,
    },
    tokens::{
        Ctx, CtxNode, DefSite, Delim, Edition, LitKind, Mark, MarkOf, Raw, Spacing, Text,
        TokenKind, TokenStream, Transparency,
    },
};

/// The columns of an expansion record, in the root's order.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecordColumn {
    /// One 4-byte little-endian entry per token: its kind, spacing and the kind's fields.
    Tokens,
    /// Every token's text, in token order.
    Text,
    /// Each token's text length, LEB128.
    TextLens,
    /// Each token's context, LEB128.
    Ctxs,
    /// Each token's origin, LEB128 of `value << 3 | tag`.
    Origins,
    /// Each literal's suffix length, LEB128, in token order.
    Suffixes,
    /// The context table: per node its parent, a flags byte, and its keys.
    Contexts,
    /// The nested calls.
    Nested,
    /// The holes: per hole its token and what fills it.
    Holes,
    /// The reads: per read its kind, its subject's length and its answer.
    Reads,
    /// The reads' subjects, in order.
    ReadText,
    /// The diagnostics, their texts ranges of [`RecordColumn::Notes`].
    Diagnostics,
    Notes,
    /// The rule that matched: 0 for none, else its index plus one.
    Arm,
}

impl RecordColumn {
    const ALL: [RecordColumn; COLUMNS] = [
        RecordColumn::Tokens,
        RecordColumn::Text,
        RecordColumn::TextLens,
        RecordColumn::Ctxs,
        RecordColumn::Origins,
        RecordColumn::Suffixes,
        RecordColumn::Contexts,
        RecordColumn::Nested,
        RecordColumn::Holes,
        RecordColumn::Reads,
        RecordColumn::ReadText,
        RecordColumn::Diagnostics,
        RecordColumn::Notes,
        RecordColumn::Arm,
    ];
}

pub(crate) const COLUMNS: usize = 14;

/// A stream record's root: each column's bytes.
type Root = [Slice<u8>; COLUMNS];

/// An expansion record's root: each column's bytes, then its token record
/// ([`crate::token_record`], a blaze-tokens buffer of [`blaze_tokens::TOKENS_SCHEMA`] at a
/// multiple of 16 bytes, opened where it is).
type ExpansionRoot = [Slice<u8>; COLUMNS + 1];

/// The root slot an expansion record's token record is in.
const TOKENS_SLOT: usize = COLUMNS;

/// The record's schema: its name, its version and the root's layout.
pub const EXPANSION_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    Part::Str("blaze-expand Expansion"),
    // 2: `NoRuleMatched` holds the closest rule (`closest`), 2026-11-16.
    // 3: the root's last slot is the expansion's token record (drive.md D122), 2026-11-17. A
    // record of an earlier version fails `open_as` and is made again (rule 26).
    Part::U64(3),
    Part::Schema(<ExpansionRoot as Record>::SCHEMA),
]);

/// Why bytes are not an expansion record.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RecordError {
    /// Not a buffer of [`EXPANSION_SCHEMA`], or its root does not check.
    Format(FormatError),
    /// The entry at byte `at` of `column` is not one this crate writes: `what` says why.
    Invalid { column: RecordColumn, at: usize, what: &'static str },
    /// The tokens or contexts do not make a stream (a context not yet in the table, a table
    /// past `u32::MAX`).
    Stream { column: RecordColumn, error: StreamError },
    /// The record's token record does not open.
    Tokens { error: OpenError },
    /// The record's token record holds `tokens` tokens where the stream's columns make `words`
    /// parser words.
    TokenCount { tokens: u32, words: u32 },
}

impl From<FormatError> for RecordError {
    fn from(error: FormatError) -> RecordError {
        RecordError::Format(error)
    }
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordError::Format(error) => write!(f, "an expansion record: {error}"),
            RecordError::Invalid { column, at, what } => {
                write!(f, "an expansion record's {column:?} column, at byte {at}: {what}")
            }
            RecordError::Stream { column, error } => {
                write!(f, "an expansion record's {column:?} column: {error}")
            }
            RecordError::Tokens { error } => write!(f, "an expansion record's token record: {error}"),
            RecordError::TokenCount { tokens, words } => write!(
                f,
                "an expansion record's token record holds {tokens} tokens where its stream makes {words} parser words"
            ),
        }
    }
}

impl core::error::Error for RecordError {}

/// Appends `value` as LEB128.
fn leb(out: &mut AVec<'_, u8>, value: u64) {
    let mut place = [0; leb::MAX_LEN];
    let len = leb::put_u64(&mut place, value);
    out.extend_from_slice(&place[..len]);
}

fn wide(value: usize) -> u64 {
    u64::try_from(value).expect("a usize fits a u64 on every target blaze runs on")
}

fn spacing_bits(spacing: Spacing) -> u32 {
    match spacing {
        Spacing::Alone => 0,
        Spacing::Joint => 1,
        Spacing::JointHidden => 2,
    }
}

fn delim_bits(delim: Delim) -> u32 {
    match delim {
        Delim::Paren => 0,
        Delim::Brace => 1,
        Delim::Bracket => 2,
        Delim::Invisible => 3,
    }
}

fn lit_bits(kind: LitKind) -> (u32, u8) {
    match kind {
        LitKind::Byte => (0, 0),
        LitKind::Char => (1, 0),
        LitKind::Integer => (2, 0),
        LitKind::Float => (3, 0),
        LitKind::Str => (4, 0),
        LitKind::StrRaw(hashes) => (5, hashes),
        LitKind::ByteStr => (6, 0),
        LitKind::ByteStrRaw(hashes) => (7, hashes),
        LitKind::CStr => (8, 0),
        LitKind::CStrRaw(hashes) => (9, hashes),
        LitKind::Err => (10, 0),
    }
}

/// A token's kind as its 4-byte entry: bits 0..3 the kind, 3..5 the spacing, then an
/// identifier's `r#` at bit 5, a punct's character at bits 8..29, a literal's kind at 5..9 and
/// its `#`s at 9..17, a delimiter at 5..7. A literal's suffix length is in its own column.
pub fn token_word(kind: TokenKind) -> u32 {
    match kind {
        TokenKind::Ident { raw, spacing } => {
            spacing_bits(spacing) << 3 | u32::from(raw == Raw::Yes) << 5
        }
        TokenKind::Punct { char, spacing } => 1 | spacing_bits(spacing) << 3 | u32::from(char) << 8,
        TokenKind::Literal { kind, spacing, .. } => {
            let (tag, hashes) = lit_bits(kind);
            2 | spacing_bits(spacing) << 3 | tag << 5 | u32::from(hashes) << 9
        }
        TokenKind::Open(delim) => 3 | delim_bits(delim) << 5,
        TokenKind::Close(delim) => 4 | delim_bits(delim) << 5,
    }
}

fn origin_word(origin: TokenOrigin) -> u64 {
    let (tag, value) = match origin {
        TokenOrigin::Input(at) => (0, at),
        TokenOrigin::AttrArgs(at) => (1, at),
        TokenOrigin::Def(at) => (2, at),
        TokenOrigin::Params(at) => (3, at),
        TokenOrigin::Made(transparency) => (4, u32::from(transparency.byte())),
    };
    u64::from(value) << 3 | tag
}

/// An optional origin: 0 for none, else its word plus one.
fn origin_option(origin: Option<TokenOrigin>) -> u64 {
    origin.map_or(0, |origin| origin_word(origin) + 1)
}

/// The columns while they are written, each in the writer's allocator.
struct Columns<'a> {
    columns: [AVec<'a, u8>; COLUMNS],
}

impl<'a> Columns<'a> {
    fn new_in(alloc: Alloc<'a>) -> Columns<'a> {
        Columns { columns: core::array::from_fn(|_| Vec::new_in(alloc)) }
    }

    fn of(&mut self, column: RecordColumn) -> &mut AVec<'a, u8> {
        &mut self.columns[column as usize]
    }

    fn leb(&mut self, column: RecordColumn, value: u64) {
        leb(self.of(column), value);
    }

    fn put(&mut self, column: RecordColumn, bytes: &[u8]) {
        self.of(column).extend_from_slice(bytes);
    }

    /// A text of the notes: its start and length.
    fn note(&mut self, text: Text) {
        self.leb(RecordColumn::Diagnostics, u64::from(text.at));
        self.leb(RecordColumn::Diagnostics, u64::from(text.len));
    }

    fn range(&mut self, column: RecordColumn, range: Range) {
        self.leb(column, u64::from(range.start));
        self.leb(column, u64::from(range.end));
    }
}

impl Expansion<'_> {
    /// The expansion as a record: one blaze-format buffer of [`EXPANSION_SCHEMA`] in `alloc`,
    /// its columns and its token record ([`Expansion::token_record`], its origins counted in
    /// record tokens of the sources `maps` gives). Equal expansions with equal maps give equal
    /// bytes; a token whose source `maps` lacks is refused by name.
    pub fn write_record<'b>(
        &self,
        maps: &OriginMaps<'_>,
        alloc: Alloc<'b>,
    ) -> Result<RecordBytes<'b>, TokenRecordError> {
        let tokens = self.token_record(maps, alloc)?;
        let c = Columns { columns: self.write_columns(alloc) };
        Ok(finish_expansion(&c, &tokens, alloc))
    }

    /// The record's columns, each its own bytes in `alloc`, before any root: what a kept
    /// expansion's entry holds ([`crate::kept::write_entry`]), read back by
    /// [`Expansion::read_columns`].
    pub(crate) fn write_columns<'b>(&self, alloc: Alloc<'b>) -> [AVec<'b, u8>; COLUMNS] {
        let mut c = Columns::new_in(alloc);
        write_stream(&mut c, &self.tokens);
        for origin in self.origins.iter() {
            c.leb(RecordColumn::Origins, origin_word(*origin));
        }
        for nested in self.nested.iter() {
            let kind = match nested.kind {
                NestedKind::FnLike => 0,
                NestedKind::MacroRules => 1,
                NestedKind::Macro2 => 2,
            };
            let scope = match nested.scope {
                PathScope::AsWritten => 0,
                PathScope::DefCrate => 1,
            };
            c.put(RecordColumn::Nested, &[kind, scope]);
            c.range(RecordColumn::Nested, nested.path);
            c.leb(RecordColumn::Nested, nested.name.map_or(0, |name| u64::from(name) + 1));
            c.range(RecordColumn::Nested, nested.args);
            match nested.params {
                None => c.put(RecordColumn::Nested, &[0]),
                Some(params) => {
                    c.put(RecordColumn::Nested, &[1]);
                    c.range(RecordColumn::Nested, params);
                }
            }
        }
        for hole in self.holes.iter() {
            let of = match hole.of {
                HoleOf::Line => 0,
                HoleOf::Column => 1,
                HoleOf::File => 2,
                HoleOf::ModulePath => 3,
            };
            c.leb(RecordColumn::Holes, u64::from(hole.token));
            c.put(RecordColumn::Holes, &[of]);
        }
        for read in self.reads.list() {
            let kind = match read.kind {
                ReadKind::Env => 0,
                ReadKind::Cfg => 1,
                ReadKind::IncludeStr => 2,
                ReadKind::IncludeBytes => 3,
                ReadKind::Line => 4,
                ReadKind::Column => 5,
                ReadKind::File => 6,
                ReadKind::ModulePath => 7,
                ReadKind::ProcMacro => 8,
            };
            let subject = self.reads.subject(read);
            c.put(RecordColumn::Reads, &[kind]);
            c.leb(RecordColumn::Reads, wide(subject.len()));
            c.put(RecordColumn::ReadText, subject.as_bytes());
            match read.answer {
                ReadAnswer::Absent => c.put(RecordColumn::Reads, &[0]),
                ReadAnswer::Digest(digest) => {
                    c.put(RecordColumn::Reads, &[1]);
                    c.put(RecordColumn::Reads, &digest.0);
                }
                ReadAnswer::Bool(value) => c.put(RecordColumn::Reads, &[2, u8::from(value)]),
                ReadAnswer::Number(value) => {
                    c.put(RecordColumn::Reads, &[3]);
                    c.leb(RecordColumn::Reads, u64::from(value));
                }
            }
        }
        for diagnostic in self.diagnostics.iter() {
            write_diagnostic(&mut c, *diagnostic);
        }
        c.put(RecordColumn::Notes, &self.notes);
        c.leb(RecordColumn::Arm, self.arm.map_or(0, |arm| u64::from(arm) + 1));
        c.columns
    }
}

impl<'a> TokenStream<'a> {
    /// The stream alone as a record: one blaze-format buffer of [`STREAM_SCHEMA`] in `alloc`,
    /// the stream columns of an expansion record ([`RecordColumn::Tokens`] to
    /// [`RecordColumn::Contexts`]) and every other column empty. What a macro definition's
    /// body and parameters are stored as, each by its [`TokenStream::record_id`]. Equal streams
    /// give equal bytes.
    pub fn write_record<'b>(&self, alloc: Alloc<'b>) -> RecordBytes<'b> {
        let mut c = Columns::new_in(alloc);
        write_stream(&mut c, self);
        finish_columns(&c, STREAM_SCHEMA, alloc)
    }

    /// The stream a [`TokenStream::write_record`] buffer holds, in `alloc`, checked once.
    pub fn read_record(buffer: &[u8], alloc: Alloc<'a>) -> Result<TokenStream<'a>, RecordError> {
        let mut r = open_columns(buffer, STREAM_SCHEMA)?;
        let stream = read_tokens(&mut r, alloc)?;
        for col in &r.columns {
            col.done()?;
        }
        Ok(stream)
    }
}

/// The digest a stream record is stored and named by: of its bytes, in its own domain.
pub fn stream_record_id(bytes: &[u8]) -> ContentId {
    blaze_ids::Hasher::new("blaze v2 expand token stream record 1").part(bytes).finish()
}

/// The schema of a token stream stored alone ([`TokenStream::write_record`]).
pub const STREAM_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    Part::Str("blaze-expand TokenStream"),
    Part::U64(1),
    Part::Schema(<Root as Record>::SCHEMA),
]);

// A finished record's bytes: the schema's one alias (rule 30).
pub use blaze_surface::RecordBytes;

/// The stream's columns: each token's entry, text, length, context and suffix, then the
/// context table.
fn write_stream(c: &mut Columns<'_>, tokens: &TokenStream<'_>) {
    for token in tokens.tokens() {
        c.put(RecordColumn::Tokens, &token_word(token.kind).to_le_bytes());
        let text = tokens.text_of(token);
        c.put(RecordColumn::Text, text.as_bytes());
        c.leb(RecordColumn::TextLens, wide(text.len()));
        c.leb(RecordColumn::Ctxs, u64::from(token.ctx.raw()));
        if let TokenKind::Literal { suffix_len, .. } = token.kind {
            c.leb(RecordColumn::Suffixes, u64::from(suffix_len));
        }
    }
    for node in tokens.contexts() {
        let Mark { of, transparency, edition, site } = node.mark;
        c.leb(RecordColumn::Contexts, u64::from(node.parent.raw()));
        let outer = u8::from(matches!(of, MarkOf::Outer(_)));
        let module = u8::from(site.module.is_some());
        let flags = outer | transparency.byte() << 1 | edition.byte() << 3 | module << 5;
        c.put(RecordColumn::Contexts, &[flags]);
        if let MarkOf::Outer(key) = of {
            c.put(RecordColumn::Contexts, &key.0);
        }
        c.put(RecordColumn::Contexts, &site.krate.0.0);
        if let Some(module) = site.module {
            c.put(RecordColumn::Contexts, &module.0.0);
        }
    }
}

/// The columns as one buffer of `schema`: each non-empty column's bytes, then the root.
fn finish_columns<'b>(
    c: &Columns<'_>,
    schema: SchemaHash,
    alloc: Alloc<'b>,
) -> RecordBytes<'b> {
    let slots: [&[u8]; COLUMNS] = core::array::from_fn(|i| &c.columns[i][..]);
    finish_slots(&slots, None, schema, alloc)
}

/// An expansion's columns and its token record `tokens` as one buffer of [`EXPANSION_SCHEMA`].
fn finish_expansion<'b>(c: &Columns<'_>, tokens: &[u8], alloc: Alloc<'b>) -> RecordBytes<'b> {
    let slots: [&[u8]; COLUMNS + 1] = core::array::from_fn(|i| match i == TOKENS_SLOT {
        true => tokens,
        false => &c.columns[i][..],
    });
    finish_slots(&slots, Some(TOKENS_SLOT), EXPANSION_SCHEMA, alloc)
}

/// `slots` as one buffer of `schema`: each non-empty slot's bytes, then the root of one
/// [`Slice`] per slot. Slot `buffer` is a blaze-format buffer of its own, written at a multiple
/// of 16 bytes so that it opens where it is.
fn finish_slots<'b, const N: usize>(
    slots: &[&[u8]; N],
    buffer: Option<usize>,
    schema: SchemaHash,
    alloc: Alloc<'b>,
) -> RecordBytes<'b>
where
    [Slice<u8>; N]: Record,
{
    let mut out = Writer::new(WriterOpts { schema: schema, alloc: alloc, capacity: None });
    let mut places = [(0u64, 0usize); N];
    for (i, (place, bytes)) in places.iter_mut().zip(slots.iter()).enumerate() {
        if !bytes.is_empty() {
            let at = match buffer == Some(i) {
                true => out.push_buffer(bytes),
                false => out.push_slice(bytes),
            };
            *place = (at, bytes.len());
        }
    }
    let root_at = out.next::<[Slice<u8>; N]>();
    let field = wide(size_of::<Slice<u8>>());
    let root: [Slice<u8>; N] = core::array::from_fn(|i| match places[i] {
        (_, 0) => Slice::EMPTY,
        (at, len) => Slice::between(root_at + wide(i) * field, at, len),
    });
    let written = out.push(&root);
    debug_assert_eq!(written, root_at);
    out.finish(root_at)
}

/// A buffer of `schema` opened: each column's bytes, none read yet.
fn open_columns(buffer: &[u8], schema: SchemaHash) -> Result<Reader<'_>, RecordError> {
    Ok(reader(open_slots::<COLUMNS>(buffer, schema)?))
}

/// An expansion record opened: each column's bytes, none read yet, and its token record's.
fn open_expansion(buffer: &[u8]) -> Result<(Reader<'_>, &[u8]), RecordError> {
    let slots = open_slots::<{ COLUMNS + 1 }>(buffer, EXPANSION_SCHEMA)?;
    let columns: [&[u8]; COLUMNS] = core::array::from_fn(|i| slots[i]);
    Ok((reader(columns), slots[TOKENS_SLOT]))
}

/// Each slot's bytes of a buffer of `schema` whose root is `N` slices.
fn open_slots<const N: usize>(buffer: &[u8], schema: SchemaHash) -> Result<[&[u8]; N], RecordError>
where
    [Slice<u8>; N]: Record,
{
    let root = blaze_format::open_as::<[Slice<u8>; N]>(buffer, schema)?;
    let mut bytes: [&[u8]; N] = [&[]; N];
    for (place, slice) in bytes.iter_mut().zip(root.iter()) {
        *place = slice.follow(buffer)?;
    }
    Ok(bytes)
}

fn reader(bytes: [&[u8]; COLUMNS]) -> Reader<'_> {
    Reader { columns: core::array::from_fn(|i| Col { column: RecordColumn::ALL[i], bytes: bytes[i], at: 0 }) }
}

/// The token record of an expansion record ([`Expansion::write_record`]'s bytes), opened where
/// it is and checked once by blaze-tokens: what a reader of the expansion's tokens (an outline,
/// a lowering) takes, with no stream read and nothing rendered. The buffer is 16-aligned, as
/// every expansion record.
pub fn expansion_tokens(buffer: &[u8]) -> Result<Tokens<'_>, RecordError> {
    let slots = open_slots::<{ COLUMNS + 1 }>(buffer, EXPANSION_SCHEMA)?;
    Tokens::open(slots[TOKENS_SLOT]).map_err(|error| RecordError::Tokens { error })
}

fn write_diagnostic(c: &mut Columns<'_>, diagnostic: Diagnostic) {
    let column = RecordColumn::Diagnostics;
    let tag = |c: &mut Columns<'_>, tag: u8| c.put(column, &[tag]);
    let origin = |c: &mut Columns<'_>, at: Option<TokenOrigin>| c.leb(column, origin_option(at));
    match diagnostic {
        Diagnostic::NoRuleMatched { rules, closest, token, expected, at } => {
            tag(c, 0);
            c.leb(column, u64::from(rules));
            // 0: no tracker ran; else the rule's index plus one.
            c.leb(column, closest.map_or(0, |arm| u64::from(arm) + 1));
            match token {
                None => c.put(column, &[0]),
                Some(token) => {
                    c.put(column, &[1]);
                    c.note(token);
                }
            }
            c.note(expected);
            origin(c, at);
        }
        Diagnostic::Binding { message, at } => {
            tag(c, 1);
            c.note(message);
            origin(c, at);
        }
        Diagnostic::LeftoverTokens { at } => {
            tag(c, 2);
            origin(c, at);
        }
        Diagnostic::LimitExceeded => tag(c, 3),
        Diagnostic::UnexpectedToken { at } => {
            tag(c, 4);
            origin(c, at);
        }
        Diagnostic::BadDefinition { message } => {
            tag(c, 5);
            c.note(message);
        }
        Diagnostic::CompileError { message } => {
            tag(c, 6);
            c.note(message);
        }
        Diagnostic::EnvNotDefined { name, message } => {
            tag(c, 7);
            c.note(name);
            match message {
                None => c.put(column, &[0]),
                Some(message) => {
                    c.put(column, &[1]);
                    c.note(message);
                }
            }
        }
        Diagnostic::Include { path, why } => {
            let why = match why {
                IncludeFailure::Missing => 0,
                IncludeFailure::NotUtf8 => 1,
                IncludeFailure::DoesNotLex => 2,
                IncludeFailure::Outside => 3,
            };
            c.put(column, &[8, why]);
            c.note(path);
        }
        Diagnostic::Derive { message } => {
            tag(c, 9);
            c.note(message);
        }
        Diagnostic::Builtin { message, at } => {
            tag(c, 10);
            c.note(message);
            origin(c, at);
        }
        Diagnostic::ProcMacro { level, message, at } => {
            c.put(column, &[11, level.byte()]);
            c.note(message);
            origin(c, at);
        }
        Diagnostic::ProcMacroPanicked { message } => {
            tag(c, 12);
            c.note(message);
        }
    }
}

/// One column while it is read: its bytes and how far.
struct Col<'b> {
    column: RecordColumn,
    bytes: &'b [u8],
    at: usize,
}

impl<'b> Col<'b> {
    fn bad(&self, what: &'static str) -> RecordError {
        RecordError::Invalid { column: self.column, at: self.at, what }
    }

    fn more(&self) -> bool {
        self.at < self.bytes.len()
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        let start = self.at;
        leb::read_u64(self.bytes, &mut self.at).map_err(|error| RecordError::Invalid {
            column: self.column,
            at: start,
            what: error.as_str(),
        })
    }

    fn u32(&mut self) -> Result<u32, RecordError> {
        let start = self.at;
        let value = self.u64()?;
        u32::try_from(value).map_err(|_| RecordError::Invalid {
            column: self.column,
            at: start,
            what: "a number past u32::MAX",
        })
    }

    fn byte(&mut self) -> Result<u8, RecordError> {
        let byte = *self.bytes.get(self.at).ok_or_else(|| self.bad("cut off by its end"))?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, len: usize) -> Result<&'b [u8], RecordError> {
        let end = self.at.checked_add(len).filter(|&end| end <= self.bytes.len());
        let end = end.ok_or_else(|| self.bad("cut off by its end"))?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn str(&mut self, len: usize) -> Result<&'b str, RecordError> {
        let start = self.at;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes).map_err(|_| RecordError::Invalid {
            column: self.column,
            at: start,
            what: "text that is not UTF-8",
        })
    }

    fn id(&mut self) -> Result<ContentId, RecordError> {
        let bytes = self.take(32)?;
        let mut id = [0; 32];
        id.copy_from_slice(bytes);
        Ok(ContentId(id))
    }

    fn usize(&mut self) -> Result<usize, RecordError> {
        let value = self.u32()?;
        Ok(usize::try_from(value).expect("a u32 fits a usize on every target blaze runs on"))
    }

    /// The column read to its end, as every column is.
    fn done(&self) -> Result<(), RecordError> {
        match self.more() {
            true => Err(self.bad("bytes after the column's last entry")),
            false => Ok(()),
        }
    }
}

fn spacing_of(bits: u32) -> Option<Spacing> {
    [Spacing::Alone, Spacing::Joint, Spacing::JointHidden].get(bits as usize).copied()
}

fn delim_of(bits: u32) -> Option<Delim> {
    [Delim::Paren, Delim::Brace, Delim::Bracket, Delim::Invisible].get(bits as usize).copied()
}

fn lit_of(tag: u32, hashes: u8) -> Option<LitKind> {
    Some(match tag {
        0 => LitKind::Byte,
        1 => LitKind::Char,
        2 => LitKind::Integer,
        3 => LitKind::Float,
        4 => LitKind::Str,
        5 => LitKind::StrRaw(hashes),
        6 => LitKind::ByteStr,
        7 => LitKind::ByteStrRaw(hashes),
        8 => LitKind::CStr,
        9 => LitKind::CStrRaw(hashes),
        10 => LitKind::Err,
        _ => return None,
    })
}

/// The kind a token entry is, its suffix length still 0; `None` for an entry [`token_word`]
/// does not write (an unknown kind, a bit no field uses, a character that is not one).
pub fn token_of(word: u32) -> Option<TokenKind> {
    let spacing = spacing_of(word >> 3 & 0b11)?;
    let kind = match word & 0b111 {
        0 => {
            let raw = if word >> 5 & 1 == 1 { Raw::Yes } else { Raw::No };
            TokenKind::Ident { raw, spacing }
        }
        1 => TokenKind::Punct { char: char::from_u32(word >> 8)?, spacing },
        2 => {
            let hashes = u8::try_from(word >> 9 & 0xff).ok()?;
            TokenKind::Literal { kind: lit_of(word >> 5 & 0b1111, hashes)?, suffix_len: 0, spacing }
        }
        3 => TokenKind::Open(delim_of(word >> 5 & 0b11)?),
        4 => TokenKind::Close(delim_of(word >> 5 & 0b11)?),
        _ => return None,
    };
    (token_word(kind) == word).then_some(kind)
}

pub(crate) fn edition_of(bits: u64) -> Option<Edition> {
    Edition::from_byte(u8::try_from(bits).ok()?)
}

fn origin_of(word: u64) -> Option<TokenOrigin> {
    let value = u32::try_from(word >> 3).ok()?;
    Some(match word & 0b111 {
        0 => TokenOrigin::Input(value),
        1 => TokenOrigin::AttrArgs(value),
        2 => TokenOrigin::Def(value),
        3 => TokenOrigin::Params(value),
        4 => TokenOrigin::Made(Transparency::from_byte(u8::try_from(value).ok()?)?),
        _ => return None,
    })
}

/// The reader of one record: each column and how far it is read.
struct Reader<'b> {
    columns: [Col<'b>; COLUMNS],
}

impl<'b> Reader<'b> {
    fn col(&mut self, column: RecordColumn) -> &mut Col<'b> {
        &mut self.columns[column as usize]
    }
}

/// The context `raw` names: a root, or one of `nodes`, the table read so far.
pub(crate) fn ctx_of(raw: u64, nodes: &[Ctx]) -> Option<Ctx> {
    match raw.checked_sub(4) {
        None => edition_of(raw).map(Ctx::root),
        Some(node) => nodes.get(usize::try_from(node).ok()?).copied(),
    }
}

impl<'a> Expansion<'a> {
    /// The expansion a [`Expansion::write_record`] buffer holds, in `alloc`, checked once:
    /// equal to the expansion written, and refused with the column and byte otherwise. Its
    /// token record is opened by blaze-tokens and must hold one token per parser word of the
    /// stream read ([`RecordError::TokenCount`]); the expansion read holds no copy of it (read
    /// it in place: [`expansion_tokens`]). The buffer is 16-aligned, as blaze-format buffers
    /// are, so the token record inside it opens where it is.
    pub fn read_record(buffer: &[u8], alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        let (mut r, token_record) = open_expansion(buffer)?;
        let tokens = read_tokens(&mut r, alloc)?;
        let record = Tokens::open(token_record).map_err(|error| RecordError::Tokens { error })?;
        let words = crate::body::parser_words(&tokens, alloc).words.len() - parser::Tokens::PAD;
        if record.len() != words {
            return Err(RecordError::TokenCount {
                tokens: crate::tokens::index(record.len()),
                words: crate::tokens::index(words),
            });
        }
        Expansion::read_rest(r, tokens, alloc)
    }

    /// The expansion whose columns ([`Expansion::write_columns`]'s) are `columns`, checked as
    /// [`Expansion::read_record`] checks: a kept expansion's entry ([`crate::kept::KeptEntry`]).
    pub(crate) fn read_columns(columns: [&[u8]; COLUMNS], alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        let mut r = Reader { columns: core::array::from_fn(|i| Col { column: RecordColumn::ALL[i], bytes: columns[i], at: 0 }) };
        let tokens = read_tokens(&mut r, alloc)?;
        Expansion::read_rest(r, tokens, alloc)
    }

    /// `tokens`, read already where they are kept (a kept set's atoms: [`crate::kept_set`]), with
    /// the rest of an expansion's columns: `columns` with the stream's columns
    /// ([`RecordColumn::Tokens`] to [`RecordColumn::Contexts`]) empty.
    pub(crate) fn read_after_tokens(columns: [&[u8]; COLUMNS], tokens: TokenStream<'a>, alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        let r = Reader { columns: core::array::from_fn(|i| Col { column: RecordColumn::ALL[i], bytes: columns[i], at: 0 }) };
        Expansion::read_rest(r, tokens, alloc)
    }

    /// Every column after the stream's, read to its end, beside `tokens`.
    fn read_rest(mut r: Reader<'_>, tokens: TokenStream<'a>, alloc: Alloc<'a>) -> Result<Expansion<'a>, RecordError> {
        let mut exp = Expansion::new_in(alloc);
        let len = tokens.len();
        exp.tokens = tokens;
        for _ in 0..len {
            let origins = r.col(RecordColumn::Origins);
            let word = origins.u64()?;
            let origin = origin_of(word).ok_or_else(|| origins.bad("an origin of no kind"))?;
            exp.origins.push(origin);
        }
        let token_at = |col: &Col<'_>, at: u32| match (at as usize) < len {
            true => Ok(at),
            false => Err(col.bad("a token index past the expansion's tokens")),
        };
        let range = |col: &mut Col<'_>| -> Result<Range, RecordError> {
            let start = col.u32()?;
            let end = col.u32()?;
            match start <= end && (end as usize) <= len {
                true => Ok(Range { start, end }),
                false => Err(col.bad("a range of tokens the expansion does not have")),
            }
        };
        let nested = r.col(RecordColumn::Nested);
        while nested.more() {
            let kind = match nested.byte()? {
                0 => NestedKind::FnLike,
                1 => NestedKind::MacroRules,
                2 => NestedKind::Macro2,
                _ => return Err(nested.bad("a nested call of no kind")),
            };
            let scope = match nested.byte()? {
                0 => PathScope::AsWritten,
                1 => PathScope::DefCrate,
                _ => return Err(nested.bad("a nested call's path of no scope")),
            };
            let path = range(nested)?;
            let name = match nested.u32()? {
                0 => None,
                name => Some(token_at(&*nested, name - 1)?),
            };
            let args = range(nested)?;
            if args.end - args.start < 2 {
                return Err(nested.bad("a nested call's arguments without their delimiters"));
            }
            let params = match nested.byte()? {
                0 => None,
                1 => Some(range(nested)?),
                _ => return Err(nested.bad("a nested call's parameters neither absent nor given")),
            };
            exp.nested.push(NestedCall { kind, path, name, args, params, scope });
        }
        let holes = r.col(RecordColumn::Holes);
        while holes.more() {
            let token = holes.u32()?;
            let token = token_at(&*holes, token)?;
            let of = match holes.byte()? {
                0 => HoleOf::Line,
                1 => HoleOf::Column,
                2 => HoleOf::File,
                3 => HoleOf::ModulePath,
                _ => return Err(holes.bad("a hole of no kind")),
            };
            exp.holes.push(Hole { token, of });
        }
        read_reads(&mut r, &mut exp)?;
        let notes = r.col(RecordColumn::Notes);
        exp.notes.extend_from_slice(notes.bytes);
        notes.at = notes.bytes.len();
        read_diagnostics(&mut r, &mut exp)?;
        exp.arm = r.col(RecordColumn::Arm).u32()?.checked_sub(1);
        for col in &r.columns {
            col.done()?;
        }
        Ok(exp)
    }
}

/// The token stream: the context table first, then each token.
fn read_tokens<'a>(
    r: &mut Reader<'_>,
    alloc: Alloc<'a>,
) -> Result<TokenStream<'a>, RecordError> {
    let mut stream = TokenStream::new_in(alloc);
    let mut nodes: AVec<'a, Ctx> = Vec::new_in(alloc);
    let contexts = r.col(RecordColumn::Contexts);
    while contexts.more() {
        let parent = contexts.u64()?;
        let parent =
            ctx_of(parent, &nodes).ok_or_else(|| contexts.bad("a parent context not before it"))?;
        let flags = contexts.byte()?;
        let transparency = Transparency::from_byte(flags >> 1 & 0b11)
            .ok_or_else(|| contexts.bad("a mark of no transparency"))?;
        let edition = edition_of(u64::from(flags >> 3 & 0b11))
            .ok_or_else(|| contexts.bad("a mark of no edition"))?;
        if flags >> 6 != 0 {
            return Err(contexts.bad("a mark's flags with bits set no field uses"));
        }
        let of = match flags & 1 {
            0 => MarkOf::This,
            _ => MarkOf::Outer(contexts.id()?),
        };
        let krate = CrateItemKey(contexts.id()?);
        let module = match flags >> 5 & 1 {
            0 => None,
            _ => Some(ItemKey(contexts.id()?)),
        };
        let mark = Mark { of, transparency, edition, site: DefSite { krate, module } };
        let at = contexts.at;
        let ctx = stream
            .context(CtxNode { parent, mark })
            .map_err(|error| RecordError::Stream { column: RecordColumn::Contexts, error })?;
        if ctx.node() != Some(crate::tokens::index(nodes.len())) {
            return Err(RecordError::Invalid {
                column: RecordColumn::Contexts,
                at,
                what: "a context the table already holds",
            });
        }
        nodes.push(ctx);
    }
    let words = r.col(RecordColumn::Tokens).bytes;
    if words.len() % 4 != 0 {
        return Err(r.col(RecordColumn::Tokens).bad("a token entry cut off by the column's end"));
    }
    for word in words.chunks_exact(4) {
        let entries = r.col(RecordColumn::Tokens);
        let raw = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        let mut kind =
            token_of(raw).ok_or_else(|| entries.bad("a token entry this crate does not write"))?;
        entries.at += 4;
        let len = r.col(RecordColumn::TextLens).usize()?;
        let text = r.col(RecordColumn::Text).str(len)?;
        let ctxs = r.col(RecordColumn::Ctxs);
        let raw_ctx = ctxs.u64()?;
        let ctx = ctx_of(raw_ctx, &nodes)
            .ok_or_else(|| ctxs.bad("a token's context its table does not have"))?;
        if let TokenKind::Literal { kind: lit, spacing, .. } = kind {
            let suffixes = r.col(RecordColumn::Suffixes);
            let suffix_len = suffixes.u32()?;
            if suffix_len as usize > text.len()
                || !text.is_char_boundary(text.len() - suffix_len as usize)
            {
                return Err(suffixes.bad("a literal's suffix longer than its text"));
            }
            kind = TokenKind::Literal { kind: lit, suffix_len, spacing };
        }
        stream
            .push(kind, text, ctx)
            .map_err(|error| RecordError::Stream { column: RecordColumn::Tokens, error })?;
    }
    Ok(stream)
}

fn read_reads(
    r: &mut Reader<'_>,
    exp: &mut Expansion<'_>,
) -> Result<(), RecordError> {
    while r.col(RecordColumn::Reads).more() {
        let reads = r.col(RecordColumn::Reads);
        let kind = match reads.byte()? {
            0 => ReadKind::Env,
            1 => ReadKind::Cfg,
            2 => ReadKind::IncludeStr,
            3 => ReadKind::IncludeBytes,
            4 => ReadKind::Line,
            5 => ReadKind::Column,
            6 => ReadKind::File,
            7 => ReadKind::ModulePath,
            8 => ReadKind::ProcMacro,
            _ => return Err(reads.bad("a read of no kind")),
        };
        let len = reads.usize()?;
        let answer = match reads.byte()? {
            0 => ReadAnswer::Absent,
            1 => ReadAnswer::Digest(reads.id()?),
            2 => match reads.byte()? {
                0 => ReadAnswer::Bool(false),
                1 => ReadAnswer::Bool(true),
                _ => return Err(reads.bad("a bool that is neither 0 nor 1")),
            },
            3 => ReadAnswer::Number(reads.u32()?),
            _ => return Err(reads.bad("a read's answer of no kind")),
        };
        let subject = r.col(RecordColumn::ReadText).str(len)?;
        exp.reads
            .push(kind, subject, answer)
            .map_err(|error| RecordError::Stream { column: RecordColumn::Reads, error })?;
    }
    Ok(())
}

fn read_diagnostics(
    r: &mut Reader<'_>,
    exp: &mut Expansion<'_>,
) -> Result<(), RecordError> {
    let notes = &exp.notes;
    let col = r.col(RecordColumn::Diagnostics);
    // A text of the notes: a whole `str` of them, as `Text::of` reads it.
    let note = |col: &mut Col<'_>| -> Result<Text, RecordError> {
        let start = col.at;
        let at = col.u32()?;
        let len = col.u32()?;
        let end = (at as usize).checked_add(len as usize);
        let text = end.and_then(|end| notes.get(at as usize..end));
        match text.map(core::str::from_utf8) {
            Some(Ok(_)) => Ok(Text { at, len }),
            _ => Err(RecordError::Invalid {
                column: RecordColumn::Diagnostics,
                at: start,
                what: "a text that is not a str of the notes",
            }),
        }
    };
    let origin = |col: &mut Col<'_>| -> Result<Option<TokenOrigin>, RecordError> {
        match col.u64()? {
            0 => Ok(None),
            word => origin_of(word - 1).map(Some).ok_or_else(|| col.bad("an origin of no kind")),
        }
    };
    let present = |col: &mut Col<'_>| -> Result<bool, RecordError> {
        match col.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(col.bad("an option neither absent nor given")),
        }
    };
    let mut read = Vec::new_in(*exp.diagnostics.allocator());
    while col.more() {
        let diagnostic = match col.byte()? {
            0 => {
                let rules = col.u32()?;
                let closest = col.u32()?.checked_sub(1);
                let token = if present(col)? { Some(note(col)?) } else { None };
                let expected = note(col)?;
                Diagnostic::NoRuleMatched { rules, closest, token, expected, at: origin(col)? }
            }
            1 => {
                let message = note(col)?;
                Diagnostic::Binding { message, at: origin(col)? }
            }
            2 => Diagnostic::LeftoverTokens { at: origin(col)? },
            3 => Diagnostic::LimitExceeded,
            4 => Diagnostic::UnexpectedToken { at: origin(col)? },
            5 => Diagnostic::BadDefinition { message: note(col)? },
            6 => Diagnostic::CompileError { message: note(col)? },
            7 => {
                let name = note(col)?;
                let message = if present(col)? { Some(note(col)?) } else { None };
                Diagnostic::EnvNotDefined { name, message }
            }
            8 => {
                let why = match col.byte()? {
                    0 => IncludeFailure::Missing,
                    1 => IncludeFailure::NotUtf8,
                    2 => IncludeFailure::DoesNotLex,
                    3 => IncludeFailure::Outside,
                    _ => return Err(col.bad("an include failure of no kind")),
                };
                Diagnostic::Include { path: note(col)?, why }
            }
            9 => Diagnostic::Derive { message: note(col)? },
            10 => {
                let message = note(col)?;
                Diagnostic::Builtin { message, at: origin(col)? }
            }
            11 => {
                let level = ProcLevel::from_byte(col.byte()?).ok_or_else(|| col.bad("a proc macro level of no kind"))?;
                let message = note(col)?;
                Diagnostic::ProcMacro { level, message, at: origin(col)? }
            }
            12 => Diagnostic::ProcMacroPanicked { message: note(col)? },
            _ => return Err(col.bad("a diagnostic of no kind")),
        };
        read.push(diagnostic);
    }
    exp.diagnostics = read;
    Ok(())
}
