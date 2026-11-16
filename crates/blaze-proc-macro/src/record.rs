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

use std::path::Path;

use bstd::args::ArgList;
use bstd::cx::Cx;
use bstd::env::Vars;
use bstd::fs::{Content, Dir, Durable, FsError, FsKind, ReadWrite};
use bstd::io::{Io, Limits, NativeWorld, OpenError};
use bstd::mem::{Bytes, TASK_FRAME};
use core::time::Duration;
use bstd::path::Path as BPath;

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

/// What a record file ends in: the digest of every byte before it under this domain. A file
/// whose end is not its body's seal is a crash's torn write (a publish is not synced: records
/// are derived, remade by a rerun), read as a miss and replaced by the next put.
const SEAL: &str = "blaze expansion record seal 1";
const SEAL_LEN: usize = 32;

/// How long one record read or publish may take (rule 18): a record is a small file, so a
/// minute is a hang, failed by the effect layer naming the record.
const RECORD_BUDGET: Duration = Duration::from_secs(60);

/// The records' one home: a directory of the effect layer, with the world it came from.
///
/// It holds the world (`Io`), not a task's `Cx`: a `Cx` is one task's and not `Sync`, while
/// the store is shared by every worker expanding, and `blaze_expand::ExpandCx::run_proc_macro`
/// hands no `Cx` down (its 25 implementors sit in six lanes' crates). So each read or publish
/// is its own root context from this world, ending [`RECORD_BUDGET`] from its start: the
/// authority is passed in (rule 75); what it lacks is the caller's cancel (DEBT: when a run
/// carries its task's `Cx`, it takes that one; NOTES.md of bstd-1116).
pub struct OnDir {
    pub io: Io,
    pub dir: Dir<ReadWrite>,
}

impl OnDir {
    fn path(name: &RecordName) -> Result<BPath, FilesError> {
        BPath::new(name.as_str()).map_err(|_| FilesError::Name { name: *name })
    }

    fn cx(&self, name: &RecordName) -> Result<Cx, FilesError> {
        self.io.cx(RECORD_BUDGET, TASK_FRAME).map_err(|_| FilesError::Stopped { name: *name })
    }

    /// The bytes at `name`, none when nothing is there.
    fn read(&self, name: &RecordName) -> Result<Option<Vec<u8>>, FilesError> {
        let cx = self.cx(name)?;
        let mut into = Bytes::new_in(cx.frame(), "an expansion record read back");
        match self.dir.read_all(&cx, &Self::path(name)?, &mut into) {
            Ok(_) => Ok(Some(into.as_slice().to_vec())),
            Err(error) if error.kind == FsKind::NotFound => Ok(None),
            Err(error) => Err(FilesError::Effect(error)),
        }
    }

    /// `name` made to hold `body` then `seal`, replaced all at once for a reader (never a
    /// part), nothing synced: after a power cut it holds what it held, or a torn part.
    fn publish(&self, name: &RecordName, body: &[u8], seal: &[u8; SEAL_LEN]) -> Result<(), FilesError> {
        let cx = self.cx(name)?;
        let mut whole = Bytes::new_in(cx.frame(), "an expansion record and its seal");
        whole.try_extend(body).and_then(|()| whole.try_extend(seal)).map_err(|_| FilesError::Stopped { name: *name })?;
        self.dir.publish(&cx, &Self::path(name)?, Content::bytes(whole.as_slice()), Durable::No).map(|_| ()).map_err(FilesError::Effect)
    }
}

/// A world over the folder `store` for a program's start (an example, a test), and that
/// folder, its root. A command's driver hands its own world instead.
pub fn native_world(store: &Path) -> Result<(Io, Dir<ReadWrite>), FilesError> {
    let io = Io::native(NativeWorld {
        root: store,
        threads: 1,
        env: Vars::given(Vec::new()),
        args: ArgList::given(Vec::new()),
        host_name: std::sync::Arc::from(""),
        seed: Some(0),
        above: 0,
        limits: Limits::USUAL,
    })
    .map_err(FilesError::World)?;
    let root = io.need_fs().map_err(|_| FilesError::NoTree)?.root();
    Ok((io, root))
}

/// A record file that could not be read or written, naming it.
#[derive(Debug)]
pub enum FilesError {
    /// The world over the store's folder could not be opened (the names examples' wrapper).
    World(OpenError),
    /// The world has no file tree to hold the records.
    NoTree,
    Effect(FsError),
    /// A record's name the effect layer refuses as a path.
    Name { name: RecordName },
    /// `name` holds a whole record of other bytes than the put's (rule 21).
    OtherBytes { name: RecordName },
    /// The task's frame refused the bytes of `name`'s record.
    Stopped { name: RecordName },
}

impl std::fmt::Display for FilesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilesError::World(error) => write!(f, "the records' folder: {error}"),
            FilesError::NoTree => f.write_str("the records' world has no file tree"),
            FilesError::Effect(error) => write!(f, "a record: {error}"),
            FilesError::Name { name } => write!(f, "the record name {name} is no path under the store"),
            FilesError::Stopped { name } => write!(f, "the record {name}: the task's frame refused its bytes"),
            FilesError::OtherBytes { name } => write!(f, "the record {name} holds other bytes than its put's (rule 21)"),
        }
    }
}

/// A planted bug the crash simulation must catch (`record_tests.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordPlant {
    None,
    /// A read takes a file without checking its seal: a torn record is handed back.
    SealSkipped,
}

/// The folder store of records: each file the record's bytes then their [`SEAL`], published
/// whole (a reader sees all or none) and taken back only when the seal matches.
pub struct DirStore {
    files: OnDir,
    tally: Tally,
    plant: RecordPlant,
}

impl DirStore {
    /// The expansion records, `proc-macro/records` beneath the store's folder `store`.
    pub fn expansions(io: &Io, store: &Dir<ReadWrite>) -> DirStore {
        DirStore::within(io, store, "proc-macro/records")
    }

    /// The build scripts' records, `build-script/records` beneath the store's folder `store`.
    pub fn build_scripts(io: &Io, store: &Dir<ReadWrite>) -> DirStore {
        DirStore::within(io, store, "build-script/records")
    }

    fn within(io: &Io, store: &Dir<ReadWrite>, at: &'static str) -> DirStore {
        let at = BPath::new(at).expect("a records folder's name is a relative path by construction");
        // clone: the world, a refcount.
        DirStore::on(OnDir { io: io.clone(), dir: store.subdir(&at) }, RecordPlant::None)
    }

    /// The expansion records under `<store>/proc-macro/records`, over a world opened here.
    /// DEBT, for removal: kept only for the names lane's examples (blaze-real
    /// examples/names_load, blaze-world examples/names.rs), which hand no world; every other
    /// caller makes [`DirStore::expansions`] from its own.
    pub fn new(store: &Path) -> DirStore {
        match native_world(store) {
            Ok((io, root)) => DirStore::expansions(&io, &root),
            Err(error) => panic!("the expansion records at {}: {error}", store.display()),
        }
    }

    /// The store over `files`; `plant` is [`RecordPlant::None`] but in the simulation.
    pub fn on(files: OnDir, plant: RecordPlant) -> DirStore {
        DirStore { files, tally: Tally::default(), plant }
    }

    fn name(kind: Kind, key: &ContentId) -> RecordName {
        let kind: &[u8] = match kind {
            Kind::List => b"list/",
            Kind::Answer => b"answer/",
        };
        let mut name = RecordName { bytes: [0; 71], len: 0 };
        name.bytes[..kind.len()].copy_from_slice(kind);
        let mut at = kind.len();
        for byte in key.0 {
            name.bytes[at] = b"0123456789abcdef"[usize::from(byte >> 4)];
            name.bytes[at + 1] = b"0123456789abcdef"[usize::from(byte & 15)];
            at += 2;
        }
        name.len = at as u8;
        name
    }
}

/// A record's file name, `list/<64 hex>` or `answer/<64 hex>`, inline: no allocation a lookup.
#[derive(Clone, Copy, Debug)]
pub struct RecordName {
    bytes: [u8; 71],
    len: u8,
}

impl RecordName {
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).expect("a record name is ASCII by construction")
    }
}

impl std::fmt::Display for RecordName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Store for DirStore {
    type Held = Vec<u8>;
    type Error = FilesError;

    fn get(&self, kind: Kind, key: &ContentId) -> Result<Option<Self::Held>, FilesError> {
        let Some(mut bytes) = self.files.read(&Self::name(kind, key))? else { return Ok(None) };
        // A torn file (any prefix of a publish, or zeros where a sector was lost) is a miss.
        let Some(body_len) = bytes.len().checked_sub(SEAL_LEN) else { return Ok(None) };
        let sealed = ContentId::of(SEAL, &bytes[..body_len]).0 == bytes[body_len..];
        if !sealed && self.plant != RecordPlant::SealSkipped {
            return Ok(None);
        }
        bytes.truncate(body_len);
        Ok(Some(bytes))
    }

    fn put(&self, kind: Kind, key: ContentId, bytes: &[u8]) -> Result<(), FilesError> {
        let name = Self::name(kind, &key);
        if let Some(held) = self.get(kind, &key)? {
            return if held == bytes { Ok(()) } else { Err(FilesError::OtherBytes { name }) };
        }
        self.files.publish(&name, bytes, &ContentId::of(SEAL, bytes).0)
    }

    fn bytes(held: &Self::Held) -> &[u8] {
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

/// The tokens of `stream` as the macro sees them: kind, spacing and text, each piece handed to
/// `out` in order (a record's buffer, or the key's hasher, which then holds no copy).
fn stream_pieces(stream: &TokenStream<'_>, mut out: impl FnMut(&[u8])) {
    out(&(stream.len() as u32).to_le_bytes());
    for token in stream.tokens() {
        out(&token_bytes(token.kind));
        let text = stream.text_of(token);
        out(&(text.len() as u32).to_le_bytes());
        out(text.as_bytes());
    }
}

fn stream_bytes(out: &mut Vec<u8>, stream: &TokenStream<'_>) {
    stream_pieces(stream, |piece| out.extend_from_slice(piece));
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
    // Streamed into the hasher, never gathered: the digest of the same bytes in the same
    // order (BLAKE3 hashes the concatenation), so the keys are those a buffer gave, and a
    // lookup allocates nothing for its key.
    let mut hasher = Hasher::new("blaze proc-macro expansion 1");
    hasher.update(&host_version.to_le_bytes()).update(b"module ").update(module_wasm_sha256.as_bytes());
    hasher.update(&export.index.to_le_bytes()).update(&[kind_byte(export.kind)]);
    hasher.update(&(export.name.len() as u32).to_le_bytes()).update(export.name.as_bytes());
    hasher.update(&[kind_byte(call.kind), edition_byte(edition)]);
    stream_pieces(call.input, |piece| {
        hasher.update(piece);
    });
    match call.attr_args {
        Some(attr) => {
            hasher.update(&[1]);
            stream_pieces(attr, |piece| {
                hasher.update(piece);
            });
        }
        None => {
            hasher.update(&[0]);
        }
    }
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
    Chain(blaze_record::Error<FilesError>),
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

impl From<blaze_record::Error<FilesError>> for RecordError {
    fn from(error: blaze_record::Error<FilesError>) -> RecordError {
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

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
