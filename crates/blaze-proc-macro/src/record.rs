//! The expansion record: what one call gave, kept by blaze-record's form `base → read-lists →
//! answer` in a write-once folder of the store.
//!
//! - The base is computed from text before the run ([`base_key`]): the module's wasm digest,
//!   the macro's index, name and kind, the edition, and the input's and the attribute's tokens
//!   as the macro sees them (kind, spacing and text; never a position or a context, which it
//!   cannot see).
//! - The read-list names what the run read (`env:NAME`, `file:path`, `dir:path`,
//!   `stat:path`, paths from the workspace root); a lookup values each now
//!   ([`crate::sandbox::answer_now`]), so a changed file or variable misses exactly the
//!   expansions that read it.
//! - The answer is the [`Produced`] in canonical bytes ([`encode`]), kept in the read-list's
//!   note after the digest each read had: a hit on a base's first list is one read of one
//!   file, its reads valued, then the decode. Two runs of one key write equal bytes, wherever
//!   the checkout is; two answers under equal reads and digests are refused (rule 21).
//!
//! The folder is `<store>/proc-macro/records/list/<id>`, the chain's slots (blaze-record's
//! [`walk`]); a put never replaces bytes, and other bytes under a held key are refused.

use std::path::{Path, PathBuf};

use blaze_expand::{
    Ctx, Delim, Edition, LitKind, ProcCall, ProcDiagnostic, ProcKind, ProcLevel, Produced, Raw, ReadAnswer, ReadKind, Reads, Spacing, Text,
    TokenKind, TokenOrigin, TokenStream, Transparency,
};
use blaze_ids::{ContentId, Hasher};
use blaze_own::{AVec, Alloc};
use blaze_record::{Bounds, Filing, Kind, ListRef, Placed, Store, Tally, Walked, walk, write_list_with};

use crate::{
    module::Exported,
    sandbox::{Package, answer_now},
};

/// The folder store of records.
pub struct DirStore {
    root: PathBuf,
    tally: Tally,
}

impl DirStore {
    /// The expansion records, under `<store>/proc-macro/records`.
    pub fn new(store: &Path) -> DirStore {
        DirStore::at(store, "proc-macro/records")
    }

    /// Records of another kind of run under `<store>/<within>` (`build-script/records`).
    pub fn at(store: &Path, within: &str) -> DirStore {
        DirStore { root: store.join(within), tally: Tally::default() }
    }

    fn path(&self, kind: Kind, key: &ContentId) -> PathBuf {
        let kind = match kind {
            Kind::List => "list",
            Kind::Answer => "answer",
        };
        let mut name = String::with_capacity(64);
        for byte in key.0 {
            name.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            name.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('0'));
        }
        self.root.join(kind).join(name)
    }
}

impl Store for DirStore {
    type Held = Vec<u8>;
    type Error = std::io::Error;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Vec<u8>>, std::io::Error> {
        match std::fs::read(self.path(kind, key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), std::io::Error> {
        let path = self.path(kind, &key);
        if let Some(held) = self.get(kind, &key)? {
            return if held == bytes {
                Ok(())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, format!("{} holds other bytes", path.display())))
            };
        }
        let dir = path.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir)?;
        let staging = dir.join(format!(".{}.{}", std::process::id(), path.file_name().and_then(|it| it.to_str()).unwrap_or("record")));
        std::fs::write(&staging, bytes)?;
        std::fs::rename(&staging, &path)
    }

    fn bytes(held: &Vec<u8>) -> &[u8] {
        held
    }

    fn tally(&self) -> &Tally {
        &self.tally
    }
}

fn kind_byte(kind: ProcKind) -> u8 {
    match kind {
        ProcKind::FnLike => 0,
        ProcKind::Attr => 1,
        ProcKind::Derive => 2,
    }
}

fn edition_byte(edition: Edition) -> u8 {
    edition as u8
}

fn delim_byte(delim: Delim) -> u8 {
    match delim {
        Delim::Paren => 0,
        Delim::Brace => 1,
        Delim::Bracket => 2,
        Delim::Invisible => 3,
    }
}

fn delim_of(byte: u8) -> Option<Delim> {
    Some(match byte {
        0 => Delim::Paren,
        1 => Delim::Brace,
        2 => Delim::Bracket,
        3 => Delim::Invisible,
        _ => return None,
    })
}

fn spacing_byte(spacing: Spacing) -> u8 {
    match spacing {
        Spacing::Alone => 0,
        Spacing::Joint => 1,
        Spacing::JointHidden => 2,
    }
}

fn spacing_of(byte: u8) -> Option<Spacing> {
    Some(match byte {
        0 => Spacing::Alone,
        1 => Spacing::Joint,
        2 => Spacing::JointHidden,
        _ => return None,
    })
}

fn lit_bytes(kind: LitKind) -> [u8; 2] {
    match kind {
        LitKind::Byte => [0, 0],
        LitKind::Char => [1, 0],
        LitKind::Integer => [2, 0],
        LitKind::Float => [3, 0],
        LitKind::Str => [4, 0],
        LitKind::StrRaw(n) => [5, n],
        LitKind::ByteStr => [6, 0],
        LitKind::ByteStrRaw(n) => [7, n],
        LitKind::CStr => [8, 0],
        LitKind::CStrRaw(n) => [9, n],
        LitKind::Err => [10, 0],
    }
}

fn lit_of(tag: u8, n: u8) -> Option<LitKind> {
    Some(match tag {
        0 => LitKind::Byte,
        1 => LitKind::Char,
        2 => LitKind::Integer,
        3 => LitKind::Float,
        4 => LitKind::Str,
        5 => LitKind::StrRaw(n),
        6 => LitKind::ByteStr,
        7 => LitKind::ByteStrRaw(n),
        8 => LitKind::CStr,
        9 => LitKind::CStrRaw(n),
        10 => LitKind::Err,
        _ => return None,
    })
}

/// A token's kind in eight bytes: tag, then its fields.
fn token_bytes(kind: TokenKind) -> [u8; 8] {
    let mut out = [0u8; 8];
    match kind {
        TokenKind::Ident { raw, spacing } => {
            out[0] = 0;
            out[1] = u8::from(raw == Raw::Yes);
            out[2] = spacing_byte(spacing);
        }
        TokenKind::Punct { char, spacing } => {
            out[0] = 1;
            out[1] = spacing_byte(spacing);
            out[4..8].copy_from_slice(&u32::from(char).to_le_bytes());
        }
        TokenKind::Literal { kind, suffix_len, spacing } => {
            out[0] = 2;
            out[1] = spacing_byte(spacing);
            out[2..4].copy_from_slice(&lit_bytes(kind));
            out[4..8].copy_from_slice(&suffix_len.to_le_bytes());
        }
        TokenKind::Open(delim) => {
            out[0] = 3;
            out[1] = delim_byte(delim);
        }
        TokenKind::Close(delim) => {
            out[0] = 4;
            out[1] = delim_byte(delim);
        }
    }
    out
}

fn token_of(bytes: [u8; 8]) -> Option<TokenKind> {
    let word = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    Some(match bytes[0] {
        0 => TokenKind::Ident { raw: if bytes[1] == 1 { Raw::Yes } else { Raw::No }, spacing: spacing_of(bytes[2])? },
        1 => TokenKind::Punct { char: char::from_u32(word)?, spacing: spacing_of(bytes[1])? },
        2 => TokenKind::Literal { kind: lit_of(bytes[2], bytes[3])?, suffix_len: word, spacing: spacing_of(bytes[1])? },
        3 => TokenKind::Open(delim_of(bytes[1])?),
        4 => TokenKind::Close(delim_of(bytes[1])?),
        _ => return None,
    })
}

/// The tokens of `stream` as the macro sees them: kind, spacing and text.
fn stream_bytes(out: &mut Vec<u8>, stream: &TokenStream<'_>) {
    out.extend_from_slice(&(stream.len() as u32).to_le_bytes());
    for token in stream.tokens() {
        out.extend_from_slice(&token_bytes(token.kind));
        let text = stream.text_of(token);
        out.extend_from_slice(&(text.len() as u32).to_le_bytes());
        out.extend_from_slice(text.as_bytes());
    }
}

/// The version of what the host gives a macro (the bridge server, the sandbox's answers, the
/// streams a call passes): in every expansion's base key, so a change to it never reads an
/// answer an older host made. 2: an attribute without arguments is given an empty stream (1
/// gave none, and every such macro's client panicked reading it). Here, beside the key, so a
/// build without the runtime computes the same keys.
pub const HOST_VERSION: u32 = 2;

/// The base of `call`'s record: computed from text, before the run.
pub fn base_key(module_wasm_sha256: &str, export: &Exported, edition: Edition, call: &ProcCall<'_>) -> ContentId {
    base_key_under(HOST_VERSION, module_wasm_sha256, export, edition, call)
}

/// [`base_key`] as the host of `host_version` computes it: an answer kept by another host
/// version is under another key, so it is never read.
pub(crate) fn base_key_under(host_version: u32, module_wasm_sha256: &str, export: &Exported, edition: Edition, call: &ProcCall<'_>) -> ContentId {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&host_version.to_le_bytes());
    bytes.extend_from_slice(b"module ");
    bytes.extend_from_slice(module_wasm_sha256.as_bytes());
    bytes.extend_from_slice(&export.index.to_le_bytes());
    bytes.push(kind_byte(export.kind));
    bytes.extend_from_slice(&(export.name.len() as u32).to_le_bytes());
    bytes.extend_from_slice(export.name.as_bytes());
    bytes.push(kind_byte(call.kind));
    bytes.push(edition_byte(edition));
    stream_bytes(&mut bytes, call.input);
    match call.attr_args {
        Some(attr) => {
            bytes.push(1);
            stream_bytes(&mut bytes, attr);
        }
        None => bytes.push(0),
    }
    let mut hasher = Hasher::new("blaze proc-macro expansion 1");
    hasher.update(&bytes);
    hasher.finish()
}

fn origin_bytes(out: &mut Vec<u8>, origin: TokenOrigin) {
    let (tag, value) = match origin {
        TokenOrigin::Input(at) => (0u8, at),
        TokenOrigin::AttrArgs(at) => (1, at),
        TokenOrigin::Def(at) => (2, at),
        TokenOrigin::Params(at) => (3, at),
        TokenOrigin::Made(Transparency::Transparent) => (4, 0),
        TokenOrigin::Made(Transparency::SemiOpaque) => (4, 1),
        TokenOrigin::Made(Transparency::Opaque) => (4, 2),
    };
    out.push(tag);
    out.extend_from_slice(&value.to_le_bytes());
}

fn origin_of(tag: u8, value: u32) -> Option<TokenOrigin> {
    Some(match (tag, value) {
        (0, at) => TokenOrigin::Input(at),
        (1, at) => TokenOrigin::AttrArgs(at),
        (2, at) => TokenOrigin::Def(at),
        (3, at) => TokenOrigin::Params(at),
        (4, 0) => TokenOrigin::Made(Transparency::Transparent),
        (4, 1) => TokenOrigin::Made(Transparency::SemiOpaque),
        (4, 2) => TokenOrigin::Made(Transparency::Opaque),
        _ => return None,
    })
}

fn level_byte(level: ProcLevel) -> u8 {
    match level {
        ProcLevel::Error => 0,
        ProcLevel::Warning => 1,
        ProcLevel::Note => 2,
        ProcLevel::Help => 3,
    }
}

fn level_of(byte: u8) -> Option<ProcLevel> {
    Some(match byte {
        0 => ProcLevel::Error,
        1 => ProcLevel::Warning,
        2 => ProcLevel::Note,
        3 => ProcLevel::Help,
        _ => return None,
    })
}

/// The layout of a record's note. 2: the answer lives in its read-list's note, after the digest
/// each read had, so a hit is one read of one file (layout 1 kept it in a second file).
const NOTE_LAYOUT: u32 = 2;

/// A read's answer as the note holds it: absent, or a digest. A proc macro's run makes no other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unkeepable {
    /// The read's subject.
    pub read: usize,
}

/// `produced` as its read-list's note, in canonical bytes: the layout, the digest each read had
/// (in the reads' order), then the tokens with their origins, the diagnostics and their text.
/// The reads' subjects are the list's own names ([`keep`]).
pub fn encode(produced: &Produced<'_>) -> Result<Vec<u8>, Unkeepable> {
    let mut out = Vec::new();
    encode_into(&mut out, produced)?;
    Ok(out)
}

fn encode_into(out: &mut Vec<u8>, produced: &Produced<'_>) -> Result<(), Unkeepable> {
    out.extend_from_slice(&NOTE_LAYOUT.to_le_bytes());
    for (at, read) in produced.reads.list().iter().enumerate() {
        match read.answer {
            ReadAnswer::Digest(id) => {
                out.push(1);
                out.extend_from_slice(&id.0);
            }
            ReadAnswer::Absent => out.push(0),
            ReadAnswer::Bool(_) | ReadAnswer::Number(_) => return Err(Unkeepable { read: at }),
        }
    }
    let out = &mut *out;
    stream_bytes(out, &produced.tokens);
    for origin in produced.origins.iter() {
        origin_bytes(out, *origin);
    }
    out.extend_from_slice(&(produced.diagnostics.len() as u32).to_le_bytes());
    for diagnostic in produced.diagnostics.iter() {
        match *diagnostic {
            ProcDiagnostic::Emitted { level, message, at } => {
                out.push(0);
                out.push(level_byte(level));
                out.extend_from_slice(&message.at.to_le_bytes());
                out.extend_from_slice(&message.len.to_le_bytes());
                match at {
                    Some(origin) => {
                        out.push(1);
                        origin_bytes(out, origin);
                    }
                    None => out.push(0),
                }
            }
            ProcDiagnostic::Panicked { message } => {
                out.push(1);
                out.extend_from_slice(&message.at.to_le_bytes());
                out.extend_from_slice(&message.len.to_le_bytes());
            }
        }
    }
    out.extend_from_slice(&(produced.text.len() as u32).to_le_bytes());
    out.extend_from_slice(&produced.text);
    Ok(())
}

/// Why a record's bytes are not one.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Corrupt(pub &'static str);

struct Cursor<'b> {
    bytes: &'b [u8],
}

impl<'b> Cursor<'b> {
    fn take(&mut self, n: usize, what: &'static str) -> Result<&'b [u8], Corrupt> {
        if self.bytes.len() < n {
            return Err(Corrupt(what));
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, Corrupt> {
        Ok(self.take(1, what)?[0])
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, Corrupt> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn str(&mut self, what: &'static str) -> Result<&'b str, Corrupt> {
        let len = self.u32(what)? as usize;
        std::str::from_utf8(self.take(len, what)?).map_err(|_| Corrupt(what))
    }

    fn origin(&mut self) -> Result<TokenOrigin, Corrupt> {
        let tag = self.u8("an origin")?;
        let value = self.u32("an origin")?;
        origin_of(tag, value).ok_or(Corrupt("an origin's tag"))
    }

    /// A read's answer as the note holds it.
    fn answer(&mut self) -> Result<ReadAnswer, Corrupt> {
        Ok(match self.u8("a read's answer")? {
            0 => ReadAnswer::Absent,
            1 => ReadAnswer::Digest(ContentId(self.take(32, "a read's digest")?.try_into().map_err(|_| Corrupt("a read's digest"))?)),
            _ => return Err(Corrupt("a read's answer tag")),
        })
    }

    /// The note's layout word, checked.
    fn layout(&mut self) -> Result<(), Corrupt> {
        match self.u32("the layout")? == NOTE_LAYOUT {
            true => Ok(()),
            false => Err(Corrupt("the layout word")),
        }
    }
}

/// The [`Produced`] a read-list holds: its reads named by `list`, their digests and the answer
/// in its note ([`encode`]'s), in `alloc`.
pub fn decode<'a>(list: ListRef<'_>, edition: Edition, alloc: Alloc<'a>) -> Result<Produced<'a>, Corrupt> {
    let mut c = Cursor { bytes: list.note() };
    c.layout()?;
    let mut reads = Reads::new_in(alloc);
    for subject in list.reads() {
        let subject = std::str::from_utf8(subject).map_err(|_| Corrupt("a read's subject"))?;
        reads.push(ReadKind::ProcMacro, subject, c.answer()?).map_err(|_| Corrupt("a read that does not fit"))?;
    }
    let ctx = Ctx::root(edition);
    let mut tokens = TokenStream::new_in(alloc);
    let count = c.u32("the token count")?;
    for _ in 0..count {
        let kind: [u8; 8] = c.take(8, "a token")?.try_into().map_err(|_| Corrupt("a token"))?;
        let kind = token_of(kind).ok_or(Corrupt("a token's kind"))?;
        let text = c.str("a token's text")?;
        tokens.push(kind, text, ctx).map_err(|_| Corrupt("a token that does not fit"))?;
    }
    let mut origins: AVec<'a, TokenOrigin> = AVec::new_in(alloc);
    for _ in 0..count {
        origins.push(c.origin()?);
    }
    let mut diagnostics: AVec<'a, ProcDiagnostic> = AVec::new_in(alloc);
    for _ in 0..c.u32("the diagnostic count")? {
        let diagnostic = match c.u8("a diagnostic")? {
            0 => {
                let level = level_of(c.u8("a level")?).ok_or(Corrupt("a level"))?;
                let message = Text { at: c.u32("a message")?, len: c.u32("a message")? };
                let at = match c.u8("a diagnostic's place")? {
                    0 => None,
                    _ => Some(c.origin()?),
                };
                ProcDiagnostic::Emitted { level, message, at }
            }
            1 => ProcDiagnostic::Panicked { message: Text { at: c.u32("a message")?, len: c.u32("a message")? } },
            _ => return Err(Corrupt("a diagnostic's tag")),
        };
        diagnostics.push(diagnostic);
    }
    let len = c.u32("the text")? as usize;
    let mut text: AVec<'a, u8> = AVec::new_in(alloc);
    text.extend_from_slice(c.take(len, "the text")?);
    if !c.bytes.is_empty() {
        return Err(Corrupt("bytes after the record's end"));
    }
    Ok(Produced { tokens, origins, reads, diagnostics, text })
}

/// Why a record could not be read or kept.
#[derive(Debug)]
pub enum RecordError {
    /// The store failed under the chain, or a slot of the base's chain holds what is no
    /// read-list of it: the error names the key or the slot and what was being done.
    Chain(blaze_record::Error<std::io::Error>),
    /// The read-list under `base` is not this layout's record.
    Corrupt { base: ContentId, what: &'static str },
    /// The run's read `read` (its index) had an answer a note does not hold.
    Unkeepable { base: ContentId, read: usize },
    /// Another answer is kept under `base` for the same reads with the same digests: the run
    /// read or depended on something its key and list do not name (rule 21).
    TwoAnswers { base: ContentId },
    /// Every generation of `base`'s chain is full ([`Placed::Full`]): the answer is not kept.
    Full { base: ContentId },
}

impl From<blaze_record::Error<std::io::Error>> for RecordError {
    fn from(error: blaze_record::Error<std::io::Error>) -> RecordError {
        RecordError::Chain(error)
    }
}

impl std::fmt::Display for RecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecordError::Chain(error) => write!(f, "the expansion records: {error}"),
            RecordError::Corrupt { base, what } => write!(f, "the expansion record of base {base:x}: {what}"),
            RecordError::Unkeepable { base, read } => {
                write!(f, "the expansion of base {base:x}: its read {read} has an answer that is neither absent nor a digest")
            }
            RecordError::TwoAnswers { base } => write!(
                f,
                "the expansion of base {base:x}: another answer is kept under the same reads and digests; the run read something its key does not name (rule 21)"
            ),
            RecordError::Full { base } => write!(f, "the expansion of base {base:x}: every generation of its chain is full; not kept"),
        }
    }
}

impl std::error::Error for RecordError {}

/// The note's digests checked against what each read of `list` is answered by now in
/// `package`: `false` at the first that differs or has no answer now. `valued` counts the reads
/// asked.
fn current(list: ListRef<'_>, package: &Package, valued: &mut u64) -> Result<bool, Corrupt> {
    let mut c = Cursor { bytes: list.note() };
    c.layout()?;
    for subject in list.reads() {
        let held = c.answer()?;
        *valued += 1;
        let Ok(subject) = std::str::from_utf8(subject) else { return Err(Corrupt("a read's subject")) };
        let Ok(now) = answer_now(package, subject) else { return Ok(false) };
        let now = now.map_or(ReadAnswer::Absent, ReadAnswer::Digest);
        if now != held {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The answer of `base` under what its reads are in `package` now, decoded in `alloc`. A hit
/// on the base's first list is one read of one file: the list holds the answer.
pub fn find<'a>(store: &DirStore, base: &ContentId, package: &Package, edition: Edition, alloc: Alloc<'a>) -> Result<Option<Produced<'a>>, RecordError> {
    let corrupt = |Corrupt(what)| RecordError::Corrupt { base: *base, what };
    let mut valued = 0;
    let walked = walk::<DirStore, Produced<'a>, RecordError, _>(store, Bounds::STANDARD, base, |_at, _bytes, list| {
        if !current(list, package, &mut valued).map_err(corrupt)? {
            return Ok(None);
        }
        decode(list, edition, alloc).map(Some).map_err(corrupt)
    });
    store.tally.valued(valued);
    Ok(match walked? {
        Walked::Found { found, .. } => Some(found),
        Walked::Passed { .. } => None,
    })
}

/// Keeps `produced` under `base`: one read-list naming its reads, whose note holds the digest
/// each had and the answer. A list of the same reads and digests with another answer already
/// under `base` is refused ([`RecordError::TwoAnswers`]).
pub fn keep(store: &DirStore, base: ContentId, produced: &Produced<'_>) -> Result<(), RecordError> {
    let reads = &produced.reads;
    let list = record_bytes(base, produced)?;
    let mine = ListRef::open(&list, &base).map_err(|why| blaze_record::Error::List { at: blaze_record::Slot { generation: 0, n: 0 }, why })?;
    // The note's head, the layout and the digests: equal heads under equal reads are one
    // key's two values when the rest differs.
    let head = 4 + reads.list().iter().map(|read| if matches!(read.answer, ReadAnswer::Digest(_)) { 33 } else { 1 }).sum::<usize>();
    let walked = walk::<DirStore, (), RecordError, _>(store, Bounds::STANDARD, &base, |_at, _bytes, held| {
        let same_reads = held.len() == mine.len() && held.reads().eq(mine.reads());
        let same_head = held.note().get(..head) == mine.note().get(..head);
        Ok((same_reads && same_head && held.note() != mine.note()).then_some(()))
    })?;
    if let Walked::Found { .. } = walked {
        return Err(RecordError::TwoAnswers { base });
    }
    let mut filing = Filing::new(base, Bounds::STANDARD);
    match filing.place(store, &list)? {
        Placed::Written(_) | Placed::Kept(_) => Ok(()),
        Placed::Full => Err(RecordError::Full { base }),
    }
}

/// The bytes of `produced`'s record under `base`: the read-list of its reads, whose note holds
/// their digests and the answer ([`encode`]). One file a hit reads whole.
pub fn record_bytes(base: ContentId, produced: &Produced<'_>) -> Result<Vec<u8>, RecordError> {
    let reads = &produced.reads;
    let mut list = Vec::new();
    let mut unkeepable = None;
    write_list_with(
        &mut list,
        &base,
        |out| {
            if let Err(Unkeepable { read }) = encode_into(out, produced) {
                unkeepable = Some(read);
            }
        },
        reads.list().iter().map(|read| reads.subject(read).as_bytes()),
    );
    if let Some(read) = unkeepable {
        return Err(RecordError::Unkeepable { base, read });
    }
    Ok(list)
}
